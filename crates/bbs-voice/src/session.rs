//! Discord Voice Gateway v8 session, Guild + 1:1 DM / Group DM call coordination,
//! UDP media transport, DAVE MLS E2EE integration, and [`VoiceEngine`] implementation.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
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
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use crate::audio_io::{
    AudioBackendKind, AudioBackendMode, AudioIoHandle, CaptureQueue, PlaybackQueue,
};
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

static RUSTLS_PROVIDER_INIT: Once = Once::new();

/// Install `rustls`'s `ring` [`rustls::crypto::CryptoProvider`] as the process-wide default
/// (idempotently, once per process) so `tokio-tungstenite` WSS connections never panic.
pub fn ensure_rustls_ring_provider() {
    RUSTLS_PROVIDER_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

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
    /// Whether joining a voice channel starts with the local microphone muted (defaults to `true`
    /// to prevent accidental hot-mic transmissions).
    pub join_muted: bool,
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
            join_muted: true,
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
    self_mute: Option<bool>,
    self_deaf: Option<bool>,
    server_mute: bool,
    server_deaf: bool,
}

struct ResolvedSignaling {
    creds: VoiceGatewayCredentials,
    self_mute: bool,
    self_deaf: bool,
    server_mute: bool,
    server_deaf: bool,
}

struct MidCallServerAction {
    guild_id: Option<Snowflake>,
    channel_id: Option<Snowflake>,
    endpoint: Option<String>,
    token: Zeroizing<String>,
}

struct SharedSessionState {
    local_user_id: Snowflake,
    local_display_name: String,
    session_gen: u64,
    active_session_id: Option<String>,
    last_signaled_target: Option<(Option<Snowflake>, Snowflake)>,
    status: Option<VoiceChannelStatus>,
    call_rings: HashMap<Snowflake, CallRingState>,
    active_ring_channel: Option<Snowflake>,
    cached_voice_states: HashMap<Snowflake, HashMap<Snowflake, VoiceStateUpdateEvent>>,
    channel_names: HashMap<Snowflake, String>,
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
            session_gen: 0,
            active_session_id: None,
            last_signaled_target: None,
            status: None,
            call_rings: HashMap::new(),
            active_ring_channel: None,
            cached_voice_states: HashMap::new(),
            channel_names: HashMap::new(),
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

    fn resolve_channel_name(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        explicit: Option<String>,
    ) -> String {
        explicit
            .filter(|n| !n.is_empty())
            .or_else(|| self.channel_names.get(&channel_id).cloned())
            .unwrap_or_else(|| {
                if guild_id.is_none() {
                    format!("DM Call {channel_id}")
                } else {
                    format!("Voice {channel_id}")
                }
            })
    }

    fn upsert_cached_voice_state(&mut self, event: &VoiceStateUpdateEvent) {
        let mut empty_channels = Vec::new();
        for (&ch_id, users) in &mut self.cached_voice_states {
            if Some(ch_id) != event.channel_id {
                users.remove(&event.user_id);
                if users.is_empty() {
                    empty_channels.push(ch_id);
                }
            }
        }
        for ch_id in empty_channels {
            self.cached_voice_states.remove(&ch_id);
        }

        if let Some(ch_id) = event.channel_id {
            self.cached_voice_states
                .entry(ch_id)
                .or_default()
                .insert(event.user_id, event.clone());
        }
    }

    fn cached_display_name(&self, channel_id: Snowflake, user_id: Snowflake) -> Option<String> {
        self.cached_voice_states
            .get(&channel_id)
            .and_then(|users| users.get(&user_id))
            .and_then(|ev| ev.display_name.clone())
            .filter(|n| !n.is_empty())
    }

    fn hydrate_cached_participants(&mut self, channel_id: Snowflake) {
        let cached: Vec<VoiceStateUpdateEvent> = self
            .cached_voice_states
            .get(&channel_id)
            .map(|users| users.values().cloned().collect())
            .unwrap_or_default();

        for ev in cached {
            if ev.user_id == self.local_user_id {
                continue;
            }
            self.ensure_participant(ev.user_id, ev.display_name.clone());
            if let Some(dave) = self.dave.as_mut() {
                dave.add_recognized_user(ev.user_id);
            }
            if let Some(status) = self.status.as_mut()
                && let Some(p) = status.participants.iter_mut().find(|p| p.user_id == ev.user_id)
            {
                p.self_mute = ev.self_mute;
                p.self_deaf = ev.self_deaf;
                p.server_mute = ev.mute;
                p.server_deaf = ev.deaf;
                p.streaming = ev.self_stream;
            }
        }
    }

    fn ensure_participant(&mut self, user_id: Snowflake, display_name: Option<String>) {
        let volume = self.user_volumes.get(&user_id).copied().unwrap_or(100);
        let dave_verified = self
            .dave
            .as_ref()
            .is_some_and(|d| d.is_user_in_verified_group(user_id));
        let active_ch = self.status.as_ref().map(|s| s.channel_id);
        let cached_name = active_ch.and_then(|ch| self.cached_display_name(ch, user_id));
        let effective_name = display_name.filter(|n| !n.is_empty()).or(cached_name);

        let Some(status) = self.status.as_mut() else {
            return;
        };

        if let Some(existing) = status.participants.iter_mut().find(|p| p.user_id == user_id) {
            if let Some(name) = effective_name {
                existing.display_name = name;
            }
            existing.dave_verified = dave_verified;
            return;
        }

        let name = effective_name.unwrap_or_else(|| {
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

    fn next_active_ring(&self) -> (Option<Snowflake>, CallRingState) {
        if let Some(ch_id) = self.status.as_ref().map(|s| s.channel_id)
            && let Some(ring) = self.call_rings.get(&ch_id)
            && !matches!(ring, CallRingState::Idle)
        {
            return (Some(ch_id), ring.clone());
        }
        if let Some((&ch_id, ring)) = self
            .call_rings
            .iter()
            .find(|(_, state)| !matches!(state, CallRingState::Idle))
        {
            return (Some(ch_id), ring.clone());
        }
        (None, CallRingState::Idle)
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
    signaling_abort: Arc<AtomicU64>,
    audio_io: Arc<AudioIoHandle>,
    playback_queue: Arc<PlaybackQueue>,
    player: VoiceMessagePlayer,
    events: broadcast::Sender<BackendEvent>,
    op_lock: tokio::sync::Mutex<()>,
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
            signaling_abort: Arc::new(AtomicU64::new(0)),
            audio_io,
            playback_queue: Arc::new(PlaybackQueue::new()),
            player,
            events,
            op_lock: tokio::sync::Mutex::new(()),
            running: tokio::sync::Mutex::new(None),
        })
    }

    /// Register or update a human-readable channel name used in [`VoiceChannelStatus`].
    pub fn set_channel_name(&self, channel_id: Snowflake, name: impl Into<String>) {
        let name = name.into();
        if name.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.state.lock() {
            guard.channel_names.insert(channel_id, name.clone());
            if let Some(status) = guard.status.as_mut()
                && status.channel_id == channel_id
            {
                status.channel_name = name;
            }
        }
    }

    /// Toggle voice connection for `(guild_id, channel_id)`: leaves if already connected/connecting
    /// to that exact channel, otherwise joins it (serialized against concurrent join/leave calls).
    pub async fn toggle_voice(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> CoreResult<Option<VoiceChannelStatus>> {
        self.abort_pending_if_different(guild_id, Some(channel_id));
        let _op = self.op_lock.lock().await;
        if self.is_live_in_channel(guild_id, channel_id).await {
            self.leave_voice_locked().await?;
            Ok(None)
        } else {
            self.join_voice_locked(guild_id, channel_id).await.map(Some)
        }
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
    /// caching pre-join occupants, syncing local/remote mute & server-mute flags, re-initializing
    /// DAVE on moderator channel moves, and cleanly tearing down on remote disconnect/hijack.
    pub async fn handle_voice_state_update(&self, event: VoiceStateUpdateEvent) {
        let mut should_leave_gen: Option<u64> = None;
        let mut notify_signaling = false;
        let mut emit_status: Option<VoiceChannelStatus> = None;
        let mut ctrl_mute: Option<bool> = None;
        let mut ctrl_deaf: Option<bool> = None;

        if let Ok(mut guard) = self.state.lock() {
            if event.user_id == guard.local_user_id {
                let matches_pending = guard.pending_signaling.target_channel_id == event.channel_id
                    && guard.pending_signaling.target_guild_id == event.guild_id;

                if matches_pending && event.channel_id.is_some() {
                    guard.pending_signaling.session_id =
                        Some(Zeroizing::new(event.session_id.clone()));
                    guard.pending_signaling.self_mute = Some(event.self_mute);
                    guard.pending_signaling.self_deaf = Some(event.self_deaf);
                    guard.pending_signaling.server_mute = event.mute;
                    guard.pending_signaling.server_deaf = event.deaf;
                    guard.active_session_id = Some(event.session_id.clone());
                    notify_signaling = true;
                } else {
                    let same_session = guard
                        .active_session_id
                        .as_deref()
                        .is_none_or(|sid| sid == event.session_id);
                    let active_scope = guard.status.as_ref().map(|status| {
                        let same_scope = status.guild_id == event.guild_id
                            || (status.guild_id.is_none()
                                && event.guild_id.is_none()
                                && (event.channel_id == Some(status.channel_id)
                                    || event.channel_id.is_none()));
                        (same_scope, status.guild_id, status.channel_id)
                    });

                    if let Some((true, active_guild_id, old_ch)) = active_scope {
                        match event.channel_id {
                            Some(new_ch) if same_session => {
                                let local_uid = guard.local_user_id;
                                let local_name = guard.local_display_name.clone();
                                let moved = new_ch != old_ch;
                                let (prev_eff_mute, prev_eff_deaf) =
                                    guard.status.as_ref().map_or((false, false), |st| {
                                        let local_p = st
                                            .participants
                                            .iter()
                                            .find(|p| p.user_id == local_uid);
                                        let srv_m = local_p.is_some_and(|p| p.server_mute);
                                        let srv_d = local_p.is_some_and(|p| p.server_deaf);
                                        (
                                            st.self_mute || srv_m || st.self_deaf || srv_d,
                                            st.self_deaf || srv_d,
                                        )
                                    });

                                let new_name = if moved {
                                    Some(guard.resolve_channel_name(active_guild_id, new_ch, None))
                                } else {
                                    None
                                };

                                if let Some(status) = guard.status.as_mut() {
                                    status.channel_id = new_ch;
                                    status.self_mute = event.self_mute;
                                    status.self_deaf = event.self_deaf;
                                    if let Some(name) = new_name {
                                        status.channel_name = name;
                                        status.connection_state =
                                            VoiceConnectionState::Reconnecting;
                                        status.participants.retain(|p| p.user_id == local_uid);
                                    }
                                }

                                if moved {
                                    guard.last_signaled_target = Some((active_guild_id, new_ch));
                                    guard.mixer.clear_speakers();
                                    guard.dave = Some(DaveController::new(local_uid, new_ch));
                                    guard.hydrate_cached_participants(new_ch);
                                }

                                guard.ensure_participant(local_uid, Some(local_name));
                                let eff_deaf = event.self_deaf || event.deaf;
                                let eff_mute = event.self_mute || event.mute || eff_deaf;
                                guard.mixer.set_deafened(eff_deaf);
                                if eff_mute {
                                    guard.mixer.update_local_mic_level(MIN_RMS_DB, false);
                                }

                                if let Some(status) = guard.status.as_mut()
                                    && let Some(me) = status
                                        .participants
                                        .iter_mut()
                                        .find(|p| p.user_id == local_uid)
                                {
                                    me.self_mute = event.self_mute;
                                    me.self_deaf = event.self_deaf;
                                    me.server_mute = event.mute;
                                    me.server_deaf = event.deaf;
                                    me.streaming = event.self_stream;
                                    if eff_mute {
                                        me.speaking = false;
                                        me.rms_db = MIN_RMS_DB;
                                    }
                                }

                                if moved {
                                    guard.sync_dave_into_status();
                                }
                                if eff_mute != prev_eff_mute {
                                    ctrl_mute = Some(eff_mute);
                                }
                                if eff_deaf != prev_eff_deaf {
                                    ctrl_deaf = Some(eff_deaf);
                                }
                                emit_status = guard.status.clone();
                            }
                            _ => {
                                // Disconnected or moved to another Discord client session.
                                should_leave_gen = Some(guard.session_gen);
                            }
                        }
                    }
                }
            } else {
                // Always maintain remote voice state cache so joining/moving into a channel
                // immediately hydrates existing participants.
                guard.upsert_cached_voice_state(&event);

                if let Some(active_ch) = guard.status.as_ref().map(|s| s.channel_id) {
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
                                if event.self_mute || event.mute || event.self_deaf || event.deaf {
                                    p.speaking = false;
                                    p.rms_db = MIN_RMS_DB;
                                }
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
        }

        if notify_signaling {
            self.signaling_notify.notify_waiters();
        }
        if (ctrl_mute.is_some() || ctrl_deaf.is_some())
            && let Some(ctrl) = self.control_sender().await
        {
            if let Some(deaf) = ctrl_deaf {
                let _ = ctrl.send(RuntimeControl::SetDeaf(deaf)).await;
            }
            if let Some(muted) = ctrl_mute {
                let _ = ctrl.send(RuntimeControl::SetMute(muted)).await;
            }
        }
        if let Some(st) = emit_status {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
        if let Some(expected_gen) = should_leave_gen {
            let _op = self.op_lock.lock().await;
            let still_matches = self
                .state
                .lock()
                .is_ok_and(|g| g.session_gen == expected_gen && g.status.is_some());
            if still_matches {
                let _ = self.stop_running_session_locked(true).await;
                if let Ok(mut guard) = self.state.lock() {
                    guard.last_signaled_target = None;
                }
            }
        }
    }

    /// Feed an inbound main Gateway `VOICE_SERVER_UPDATE` event into the engine.
    ///
    /// Works identically for Guild voice (`guild_id: Some(...)`) and 1:1 / Group DM calls
    /// (`guild_id: None`, matched via `channel_id`), handling both initial join handshakes and
    /// mid-call voice server migrations / moderator channel moves.
    pub async fn handle_voice_server_update(&self, event: VoiceServerUpdateEvent) {
        let mut notify_signaling = false;
        let mut mid_call_action: Option<MidCallServerAction> = None;

        if let Ok(mut guard) = self.state.lock() {
            let matches_pending_guild = event.guild_id.is_some()
                && guard.pending_signaling.target_guild_id == event.guild_id;
            let matches_pending_dm = event.guild_id.is_none()
                && guard.pending_signaling.target_guild_id.is_none()
                && event.channel_id.is_some()
                && guard.pending_signaling.target_channel_id == event.channel_id;

            if matches_pending_guild || matches_pending_dm {
                guard.pending_signaling.endpoint = event.endpoint;
                guard.pending_signaling.token = Some(event.token);
                notify_signaling = true;
            } else if guard.pending_signaling.target_channel_id.is_none()
                && let Some(status) = guard.status.as_ref()
            {
                let matches_active_guild =
                    event.guild_id.is_some() && status.guild_id == event.guild_id;
                let matches_active_dm = event.guild_id.is_none()
                    && status.guild_id.is_none()
                    && event.channel_id == Some(status.channel_id);

                if matches_active_guild || matches_active_dm {
                    mid_call_action = Some(MidCallServerAction {
                        guild_id: event.guild_id,
                        channel_id: event.channel_id,
                        endpoint: event.endpoint,
                        token: event.token,
                    });
                }
            }
        }
        if notify_signaling {
            self.signaling_notify.notify_waiters();
        }
        if let Some(action) = mid_call_action {
            self.handle_mid_call_server_update(action).await;
        }
    }

    async fn handle_mid_call_server_update(&self, action: MidCallServerAction) {
        let _op = self.op_lock.lock().await;

        let snapshot = {
            let Ok(mut guard) = self.state.lock() else {
                return;
            };
            let Some(status) = guard.status.as_ref() else {
                return;
            };
            let matches_active_guild =
                action.guild_id.is_some() && status.guild_id == action.guild_id;
            let matches_active_dm = action.guild_id.is_none()
                && status.guild_id.is_none()
                && action.channel_id == Some(status.channel_id);
            if !matches_active_guild && !matches_active_dm {
                return;
            }
            let Some(session_id) = guard.active_session_id.clone() else {
                return;
            };

            let local_p = status
                .participants
                .iter()
                .find(|p| p.user_id == guard.local_user_id);
            let server_mute = local_p.is_some_and(|p| p.server_mute);
            let server_deaf = local_p.is_some_and(|p| p.server_deaf);

            let info = (
                status.guild_id,
                status.channel_id,
                status.channel_name.clone(),
                status.self_mute,
                status.self_deaf,
                server_mute,
                server_deaf,
                guard.local_user_id,
                session_id,
            );

            if action.endpoint.is_none() {
                // Server is clearing the endpoint mid-migration; mark Reconnecting and drop only
                // the old WebSocket task while preserving active voice status and session_id.
                guard.session_gen = guard.session_gen.wrapping_add(1);
                if let Some(st) = guard.status.as_mut() {
                    st.connection_state = VoiceConnectionState::Reconnecting;
                    let _ = self
                        .events
                        .send(BackendEvent::VoiceStatusUpdated(st.clone()));
                }
                guard.mixer.clear_speakers();
                None
            } else {
                Some(info)
            }
        };

        let Some((
            guild_id,
            channel_id,
            channel_name,
            self_mute,
            self_deaf,
            server_mute,
            server_deaf,
            user_id,
            session_id,
        )) = snapshot
        else {
            // Endpoint was None: tear down only the active WS task & audio streams.
            self.stop_ws_task_and_audio_only().await;
            return;
        };

        let Some(endpoint) = action.endpoint else {
            return;
        };

        let creds = VoiceGatewayCredentials {
            guild_id,
            channel_id,
            channel_name: Some(channel_name),
            user_id,
            session_id: Zeroizing::new(session_id),
            token: action.token,
            endpoint,
        };

        if let Err(err) = self
            .start_voice_gateway_session_locked(
                creds,
                self_mute,
                self_deaf,
                server_mute,
                server_deaf,
                true,
            )
            .await
        {
            warn!("mid-call voice server reconnect failed: {err}");
            let _ = self.events.send(BackendEvent::ErrorBanner {
                scope: "voice".to_string(),
                message: format!("voice server reconnect failed: {err}"),
                recoverable: true,
            });
        }
    }

    /// Feed an inbound main Gateway `CALL_CREATE` or `CALL_UPDATE` event for 1:1 / Group DM calls.
    ///
    /// Ring state notifications are channel-scoped so an idle update for an unrelated channel
    /// cannot clobber an active ring on another channel.
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

        let mut emit_ring: Option<CallRingState> = None;
        let mut emit_status = None;
        if let Ok(mut guard) = self.state.lock() {
            if matches!(ring_state, CallRingState::Idle) {
                guard.call_rings.remove(&event.channel_id);
                if guard.active_ring_channel == Some(event.channel_id) {
                    let (next_ch, next_state) = guard.next_active_ring();
                    guard.active_ring_channel = next_ch;
                    emit_ring = Some(next_state);
                }
            } else {
                guard
                    .call_rings
                    .insert(event.channel_id, ring_state.clone());
                guard.active_ring_channel = Some(event.channel_id);
                emit_ring = Some(ring_state.clone());
            }

            if let Some(status) = guard.status.as_mut()
                && status.channel_id == event.channel_id
            {
                status.call_ring = ring_state;
                emit_status = Some(status.clone());
            }
        }

        if let Some(ring) = emit_ring {
            let _ = self.events.send(BackendEvent::CallRingUpdated(ring));
        }
        if let Some(st) = emit_status {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
    }

    /// Feed an inbound main Gateway `CALL_DELETE` event when a DM/Group DM call ends.
    ///
    /// Strictly channel-scoped: deleting an unrelated DM call does not overwrite an active
    /// ring banner belonging to another channel.
    pub fn handle_call_delete(&self, channel_id: Snowflake) {
        let mut emit_ring: Option<CallRingState> = None;
        let mut emit_status = None;
        if let Ok(mut guard) = self.state.lock() {
            guard.call_rings.remove(&channel_id);
            if guard.active_ring_channel == Some(channel_id) {
                let (next_ch, next_state) = guard.next_active_ring();
                guard.active_ring_channel = next_ch;
                emit_ring = Some(next_state);
            }
            if let Some(status) = guard.status.as_mut()
                && status.channel_id == channel_id
            {
                status.call_ring = CallRingState::Idle;
                emit_status = Some(status.clone());
            }
        }
        if let Some(ring) = emit_ring {
            let _ = self.events.send(BackendEvent::CallRingUpdated(ring));
        }
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
        let _op = self.op_lock.lock().await;
        self.start_voice_gateway_session_locked(
            creds,
            self.config.join_muted,
            false,
            false,
            false,
            false,
        )
        .await
    }

    async fn stop_ws_task_and_audio_only(&self) {
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
    }

    async fn start_voice_gateway_session_locked(
        &self,
        creds: VoiceGatewayCredentials,
        initial_self_mute: bool,
        initial_self_deaf: bool,
        initial_server_mute: bool,
        initial_server_deaf: bool,
        is_reconnect: bool,
    ) -> VoiceResult<VoiceChannelStatus> {
        if is_reconnect {
            // Bump session generation BEFORE stopping the superseded task so its cleanup path
            // does not emit a spurious `Disconnected` event or clobber our reconnecting status.
            if let Ok(mut guard) = self.state.lock() {
                guard.session_gen = guard.session_gen.wrapping_add(1);
            }
            self.stop_ws_task_and_audio_only().await;
        } else {
            self.stop_running_session_locked(true).await?;
        }

        let (session_gen, initial_status) = {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;

            guard.session_gen = guard.session_gen.wrapping_add(1);
            let my_gen = guard.session_gen;

            let channel_name =
                guard.resolve_channel_name(creds.guild_id, creds.channel_id, creds.channel_name.clone());
            let eff_deaf = initial_self_deaf || initial_server_deaf;
            let eff_mute = initial_self_mute || initial_server_mute || eff_deaf;

            guard.mixer.clear_speakers();
            guard.mixer.set_local_user(creds.user_id);
            guard.mixer.set_deafened(eff_deaf);
            guard.active_session_id = Some(creds.session_id.as_str().to_string());
            guard.last_signaled_target = Some((creds.guild_id, creds.channel_id));
            guard.dave = Some(DaveController::new(creds.user_id, creds.channel_id));

            let preserved_participants: Vec<VoiceParticipant> = if is_reconnect {
                guard
                    .status
                    .as_ref()
                    .filter(|st| st.channel_id == creds.channel_id)
                    .map(|st| st.participants.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

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
                connection_state: if is_reconnect {
                    VoiceConnectionState::Reconnecting
                } else {
                    VoiceConnectionState::Connecting
                },
                self_mute: initial_self_mute,
                self_deaf: initial_self_deaf,
                push_to_talk: matches!(guard.input_mode, InputMode::PushToTalk),
                ping_ms: None,
                packet_loss_pct: 0.0,
                dave: DaveSessionState::Handshaking,
                call_ring,
                participants: preserved_participants,
            };
            guard.status = Some(status);

            let local_name = guard.local_display_name.clone();
            guard.ensure_participant(creds.user_id, Some(local_name));
            guard.hydrate_cached_participants(creds.channel_id);

            // Register all known participants for this channel in the fresh DAVE MLS controller.
            let known_peers: Vec<Snowflake> = guard
                .status
                .as_ref()
                .map(|st| {
                    st.participants
                        .iter()
                        .filter(|p| p.user_id != creds.user_id)
                        .map(|p| p.user_id)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(dave) = guard.dave.as_mut() {
                for peer_uid in known_peers {
                    dave.add_recognized_user(peer_uid);
                }
            }

            if let Some(st) = guard.status.as_mut()
                && let Some(me) = st
                    .participants
                    .iter_mut()
                    .find(|p| p.user_id == creds.user_id)
            {
                me.self_mute = initial_self_mute;
                me.self_deaf = initial_self_deaf;
                me.server_mute = initial_server_mute;
                me.server_deaf = initial_server_deaf;
                if eff_mute {
                    me.speaking = false;
                    me.rms_db = MIN_RMS_DB;
                }
            }
            guard.sync_dave_into_status();

            let cloned = guard
                .status
                .clone()
                .ok_or_else(|| VoiceError::Internal("missing voice status".to_string()))?;
            (my_gen, cloned)
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

            // Only clean up audio and emit Disconnected if this task was NOT superseded by a
            // newer session or mid-call reconnect.
            let is_current = state
                .lock()
                .is_ok_and(|guard| guard.session_gen == session_gen);
            if is_current {
                let _ = audio_io.stop_capture().await;
                let _ = audio_io.stop_playback().await;
                playback_queue.clear();

                if let Ok(mut guard) = state.lock()
                    && guard.session_gen == session_gen
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

    async fn is_live_in_channel(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> bool {
        let task_alive = {
            let running = self.running.lock().await;
            running.as_ref().is_some_and(|r| !r.task.is_finished())
        };
        if !task_alive {
            return false;
        }
        self.state.lock().is_ok_and(|g| {
            g.status.as_ref().is_some_and(|st| {
                st.guild_id == guild_id
                    && st.channel_id == channel_id
                    && st.connection_state != VoiceConnectionState::Disconnected
            })
        })
    }

    fn abort_pending_if_different(
        &self,
        target_guild_id: Option<Snowflake>,
        target_channel_id: Option<Snowflake>,
    ) {
        let should_abort = self.state.lock().is_ok_and(|g| {
            g.pending_signaling.target_channel_id.is_some()
                && (g.pending_signaling.target_guild_id != target_guild_id
                    || g.pending_signaling.target_channel_id != target_channel_id)
        });
        if should_abort {
            self.signaling_abort.fetch_add(1, Ordering::SeqCst);
            self.signaling_notify.notify_waiters();
        }
    }

    async fn stop_running_session_locked(&self, emit_disconnect: bool) -> VoiceResult<()> {
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

        let disconnect_event = if let Ok(mut guard) = self.state.lock() {
            let ev = if emit_disconnect
                && let Some(mut st) = guard.status.take()
                && st.connection_state != VoiceConnectionState::Disconnected
            {
                st.connection_state = VoiceConnectionState::Disconnected;
                Some(st)
            } else {
                guard.status = None;
                None
            };
            guard.session_gen = guard.session_gen.wrapping_add(1);
            guard.dave = None;
            guard.active_session_id = None;
            guard.mixer.clear_speakers();
            guard.pending_signaling = PendingSignaling::default();
            ev
        } else {
            None
        };

        if let Some(st) = disconnect_event {
            let _ = self.events.send(BackendEvent::VoiceStatusUpdated(st));
        }
        Ok(())
    }

    async fn wait_for_signaling_credentials(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        abort_epoch: u64,
    ) -> VoiceResult<ResolvedSignaling> {
        let deadline = tokio::time::Instant::now() + self.config.signaling_timeout;
        loop {
            // Register and enable the waiter BEFORE checking shared state to prevent lost wakeups.
            let notified = self.signaling_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.signaling_abort.load(Ordering::SeqCst) != abort_epoch {
                return Err(VoiceError::Signaling(
                    "voice join cancelled by newer voice operation".to_string(),
                ));
            }

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
                        (Some(sid), Some(endpoint), Some(token)) => Some(ResolvedSignaling {
                            creds: VoiceGatewayCredentials {
                                guild_id,
                                channel_id,
                                channel_name: guard.channel_names.get(&channel_id).cloned(),
                                user_id: guard.local_user_id,
                                session_id: sid.clone(),
                                token: token.clone(),
                                endpoint: endpoint.clone(),
                            },
                            self_mute: guard
                                .pending_signaling
                                .self_mute
                                .unwrap_or(self.config.join_muted),
                            self_deaf: guard.pending_signaling.self_deaf.unwrap_or(false),
                            server_mute: guard.pending_signaling.server_mute,
                            server_deaf: guard.pending_signaling.server_deaf,
                        }),
                        _ => None,
                    }
                } else {
                    None
                }
            };

            if let Some(resolved) = maybe_ready {
                return Ok(resolved);
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(VoiceError::Signaling(format!(
                    "timed out waiting for VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE for channel {channel_id}"
                )));
            }
        }
    }

    async fn join_voice_locked(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> CoreResult<VoiceChannelStatus> {
        // Idempotent guard: clicking the already-active voice channel must NOT tear down and
        // re-send an identical Op 4 (Discord ignores identical Op 4, causing a 15s timeout).
        if self.is_live_in_channel(guild_id, channel_id).await
            && let Ok(guard) = self.state.lock()
            && let Some(existing) = guard.status.clone()
        {
            return Ok(existing);
        }

        let prev_signaled_same_channel = self
            .state
            .lock()
            .ok()
            .and_then(|g| g.last_signaled_target)
            == Some((guild_id, channel_id));

        self.stop_running_session_locked(true)
            .await
            .map_err(CoreError::from)?;

        let Some(bridge) = self.signaling.as_ref() else {
            return Err(CoreError::Voice(
                "no main gateway signaling bridge configured; use connect_voice_gateway for direct connections".to_string(),
            ));
        };

        // If retrying the exact same channel after a dead Voice WebSocket while Discord still
        // holds our voice state there, send an Op 4 leave first so Discord replies to the rejoin.
        if prev_signaled_same_channel {
            let _ = bridge
                .send_voice_state_update(VoiceStateUpdateCommand {
                    guild_id,
                    channel_id: None,
                    self_mute: false,
                    self_deaf: false,
                })
                .await;
            if let Ok(mut guard) = self.state.lock() {
                guard.last_signaled_target = None;
            }
        }

        let abort_epoch = self.signaling_abort.load(Ordering::SeqCst);
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
                self_mute: None,
                self_deaf: None,
                server_mute: false,
                server_deaf: false,
            };
        }

        if let Err(err) = bridge
            .send_voice_state_update(VoiceStateUpdateCommand {
                guild_id,
                channel_id: Some(channel_id),
                self_mute: self.config.join_muted,
                self_deaf: false,
            })
            .await
        {
            if let Ok(mut guard) = self.state.lock() {
                guard.pending_signaling = PendingSignaling::default();
            }
            return Err(CoreError::from(err));
        }

        if let Ok(mut guard) = self.state.lock() {
            guard.last_signaled_target = Some((guild_id, channel_id));
        }

        let resolved = match self
            .wait_for_signaling_credentials(guild_id, channel_id, abort_epoch)
            .await
        {
            Ok(res) => res,
            Err(err) => {
                let preempted = self.signaling_abort.load(Ordering::SeqCst) != abort_epoch;
                if let Ok(mut guard) = self.state.lock() {
                    guard.pending_signaling = PendingSignaling::default();
                    if !preempted {
                        guard.last_signaled_target = None;
                    }
                }
                if !preempted {
                    // Clear any half-established voice state on Discord so we do not linger as a ghost.
                    let _ = bridge
                        .send_voice_state_update(VoiceStateUpdateCommand {
                            guild_id,
                            channel_id: None,
                            self_mute: false,
                            self_deaf: false,
                        })
                        .await;
                }
                return Err(CoreError::from(err));
            }
        };

        if let Ok(mut guard) = self.state.lock() {
            guard.pending_signaling = PendingSignaling::default();
        }

        match self
            .start_voice_gateway_session_locked(
                resolved.creds,
                resolved.self_mute,
                resolved.self_deaf,
                resolved.server_mute,
                resolved.server_deaf,
                false,
            )
            .await
        {
            Ok(status) => Ok(status),
            Err(err) => {
                if let Ok(mut guard) = self.state.lock() {
                    guard.last_signaled_target = None;
                }
                let _ = bridge
                    .send_voice_state_update(VoiceStateUpdateCommand {
                        guild_id,
                        channel_id: None,
                        self_mute: false,
                        self_deaf: false,
                    })
                    .await;
                Err(CoreError::from(err))
            }
        }
    }

    async fn leave_voice_locked(&self) -> CoreResult<()> {
        let prev_guild = self.state.lock().ok().and_then(|g| {
            g.status
                .as_ref()
                .map(|s| s.guild_id)
                .or_else(|| g.last_signaled_target.map(|(gid, _)| gid))
        });

        self.stop_running_session_locked(true)
            .await
            .map_err(CoreError::from)?;

        if let Ok(mut guard) = self.state.lock() {
            guard.last_signaled_target = None;
        }

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
}

#[async_trait]
impl VoiceEngine for DiscordVoiceEngine {
    async fn join_voice(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> CoreResult<VoiceChannelStatus> {
        self.abort_pending_if_different(guild_id, Some(channel_id));
        let _op = self.op_lock.lock().await;
        self.join_voice_locked(guild_id, channel_id).await
    }

    async fn leave_voice(&self) -> CoreResult<()> {
        self.signaling_abort.fetch_add(1, Ordering::SeqCst);
        self.signaling_notify.notify_waiters();
        let _op = self.op_lock.lock().await;
        self.leave_voice_locked().await
    }

    async fn set_self_mute(&self, muted: bool) -> CoreResult<VoiceChannelStatus> {
        let (status, guild_id, channel_id, self_deaf, eff_mute, eff_deaf) = {
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
            let mut srv_mute = false;
            let mut srv_deaf = false;
            if let Some(me) = status.participants.iter_mut().find(|p| p.user_id == local_uid) {
                srv_mute = me.server_mute;
                srv_deaf = me.server_deaf;
                me.self_mute = muted;
                me.self_deaf = deaf;
                if muted || srv_mute || deaf || srv_deaf {
                    me.speaking = false;
                    me.rms_db = MIN_RMS_DB;
                }
            }
            let eff_d = deaf || srv_deaf;
            let eff_m = muted || srv_mute || eff_d;
            let cloned = status.clone();
            let gid = status.guild_id;
            let cid = status.channel_id;
            guard.mixer.set_deafened(eff_d);
            (cloned, gid, cid, deaf, eff_m, eff_d)
        };

        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetMute(eff_mute)).await;
            if !muted {
                let _ = ctrl.send(RuntimeControl::SetDeaf(eff_deaf)).await;
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
        let (status, guild_id, channel_id, self_mute, eff_mute, eff_deaf) = {
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
            let mut srv_mute = false;
            let mut srv_deaf = false;
            if let Some(me) = status.participants.iter_mut().find(|p| p.user_id == local_uid) {
                srv_mute = me.server_mute;
                srv_deaf = me.server_deaf;
                me.self_deaf = deafened;
                me.self_mute = mute;
                if deafened || srv_deaf || mute || srv_mute {
                    me.speaking = false;
                    me.rms_db = MIN_RMS_DB;
                }
            }
            let eff_d = deafened || srv_deaf;
            let eff_m = mute || srv_mute || eff_d;
            let cloned = status.clone();
            let gid = status.guild_id;
            let cid = status.channel_id;
            guard.mixer.set_deafened(eff_d);
            (cloned, gid, cid, mute, eff_m, eff_d)
        };

        if let Some(ctrl) = self.control_sender().await {
            let _ = ctrl.send(RuntimeControl::SetDeaf(eff_deaf)).await;
            if eff_mute {
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
    ensure_rustls_ring_provider();

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
    let (mut self_muted, mut self_deafened, mut mic_processor) = {
        let guard = state
            .lock()
            .map_err(|_| VoiceError::Internal("voice state lock poisoned".to_string()))?;
        let (muted, deaf) = guard.status.as_ref().map_or((false, false), |st| {
            let local_p = st
                .participants
                .iter()
                .find(|p| p.user_id == guard.local_user_id);
            let srv_mute = local_p.is_some_and(|p| p.server_mute);
            let srv_deaf = local_p.is_some_and(|p| p.server_deaf);
            let eff_deaf = st.self_deaf || srv_deaf;
            let eff_mute = st.self_mute || srv_mute || eff_deaf;
            (eff_mute, eff_deaf)
        });
        let mut proc = MicCaptureProcessor::new(guard.input_mode)?;
        proc.set_ptt_active(guard.ptt_active);
        proc.set_high_pass_enabled(guard.high_pass_enabled);
        (muted, deaf, proc)
    };
    let mut last_speaking_sent = false;
    let mut last_seq_ack: Option<u64> = None;
    let mut pending_heartbeat: Option<(u64, Instant)> = None;
    let mut heartbeat_interval: Option<tokio::time::Interval> = None;
    let mut capture_ready_rx: Option<oneshot::Receiver<AudioBackendKind>> = None;

    let mut udp_socket: Option<UdpSocket> = None;
    let mut cipher: Option<RtpTransportCipher> = None;
    let mut packetizer: Option<RtpPacketizer> = None;
    let mut udp_rx_buf = vec![0u8; 4096];
    let mut mix_buf = [0.0f32; FRAME_SAMPLES];
    let mut mix_ticker = tokio::time::interval(Duration::from_millis(20));
    mix_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut vu_tick_counter = 0u32;

    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    let _ = ws_tx.close().await;
                    break;
                }
            }

            cap_kind = async {
                if let Some(rx) = capture_ready_rx.as_mut() {
                    rx.await
                } else {
                    std::future::pending().await
                }
            } => {
                capture_ready_rx = None;
                if let Ok(AudioBackendKind::Headless) = cap_kind
                    && last_speaking_sent
                    && let Some(pkt) = packetizer.as_ref()
                {
                    debug!("capture backend is headless; clearing initial SPEAKING indicator");
                    let spk = build_speaking_payload(0, 0, pkt.ssrc());
                    let _ = ws_tx.send(WsMessage::Text(spk.to_string().into())).await;
                    last_speaking_sent = false;
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
                                    if self_muted {
                                        // Immediately clear the speaking indicator when joined muted
                                        // so peers do not see a stuck green speaking ring.
                                        let clear_spk = build_speaking_payload(0, 0, pkt.ssrc());
                                        ws_tx
                                            .send(WsMessage::Text(clear_spk.to_string().into()))
                                            .await
                                            .map_err(|e| VoiceError::Gateway(format!("SPEAKING(0) send failed: {e}")))?;
                                        last_speaking_sent = false;
                                    } else {
                                        last_speaking_sent = true;
                                    }
                                }

                                // Start audio I/O asynchronously off the select! loop so slow ALSA/PipeWire
                                // device probing never blocks heartbeats, RTP, or 20 ms mix ticks.
                                if !self_deafened {
                                    audio_io.request_start_playback(Arc::clone(&playback_queue));
                                }
                                if !self_muted {
                                    capture_ready_rx =
                                        Some(audio_io.request_start_capture(Arc::clone(&capture_queue)));
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
                                    let is_speaking = data
                                        .get("speaking")
                                        .and_then(|v| {
                                            v.as_u64().map(|n| n != 0).or_else(|| v.as_bool())
                                        })
                                        .unwrap_or(false);

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
                            capture_ready_rx = None;
                            audio_io.request_stop_capture();
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
                        } else if packetizer.is_some() {
                            capture_ready_rx =
                                Some(audio_io.request_start_capture(Arc::clone(&capture_queue)));
                        }
                    }
                    RuntimeControl::SetDeaf(deaf) => {
                        self_deafened = deaf;
                        if deaf {
                            audio_io.request_stop_playback();
                            playback_queue.clear();
                        } else if packetizer.is_some() {
                            audio_io.request_start_playback(Arc::clone(&playback_queue));
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

                if mixed_any && !self_deafened {
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

    #[test]
    fn rustls_ring_provider_allows_client_config_builder() {
        ensure_rustls_ring_provider();
        ensure_rustls_ring_provider();
        let _builder = rustls::ClientConfig::builder();
    }

    #[tokio::test]
    async fn join_voice_is_idempotent_and_toggle_voice_and_ghost_cleanup() {
        use tokio::net::TcpListener;

        struct RecordingBridge {
            cmds: Mutex<Vec<VoiceStateUpdateCommand>>,
            endpoint: Mutex<Option<String>>,
            engine: Mutex<Option<Arc<DiscordVoiceEngine>>>,
            user_id: Snowflake,
        }

        #[async_trait]
        impl VoiceSignalingBridge for RecordingBridge {
            async fn send_voice_state_update(
                &self,
                cmd: VoiceStateUpdateCommand,
            ) -> VoiceResult<()> {
                self.cmds.lock().expect("lock").push(cmd.clone());
                let ep = self.endpoint.lock().expect("lock").clone();
                let eng = self.engine.lock().expect("lock").clone();
                if let (Some(ch), Some(endpoint), Some(engine)) = (cmd.channel_id, ep, eng) {
                    let uid = self.user_id;
                    tokio::spawn(async move {
                        engine
                            .handle_voice_state_update(VoiceStateUpdateEvent {
                                guild_id: cmd.guild_id,
                                channel_id: Some(ch),
                                user_id: uid,
                                session_id: "sess-1".to_string(),
                                self_mute: cmd.self_mute,
                                self_deaf: cmd.self_deaf,
                                mute: false,
                                deaf: false,
                                self_stream: false,
                                display_name: None,
                            })
                            .await;
                        engine
                            .handle_voice_server_update(VoiceServerUpdateEvent {
                                guild_id: cmd.guild_id,
                                channel_id: Some(ch),
                                endpoint: Some(endpoint),
                                token: Zeroizing::new("tok-1".to_string()),
                            })
                            .await;
                    });
                }
                Ok(())
            }
        }

        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                        let (mut tx, mut rx) = ws.split();
                        let _ = tx
                            .send(WsMessage::Text(
                                json!({
                                    "op": VOICE_OP_HELLO,
                                    "d": { "heartbeat_interval": 30_000, "v": 8 }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await;
                        while let Some(Ok(_)) = rx.next().await {}
                    }
                });
            }
        });

        let user_id = Snowflake(42);
        let guild_id = Snowflake(100);
        let ch_id = Snowflake(200);
        let bridge = Arc::new(RecordingBridge {
            cmds: Mutex::new(Vec::new()),
            endpoint: Mutex::new(Some(format!("ws://{addr}"))),
            engine: Mutex::new(None),
            user_id,
        });

        let cfg = VoiceEngineConfig {
            audio_mode: AudioBackendMode::Headless,
            signaling_timeout: Duration::from_millis(250),
            ..VoiceEngineConfig::new(user_id, "SysOp")
        };
        assert!(cfg.join_muted, "VoiceEngineConfig::new must default join_muted = true");
        let engine = Arc::new(
            DiscordVoiceEngine::new(
                cfg,
                Some(Arc::clone(&bridge) as Arc<dyn VoiceSignalingBridge>),
            )
            .expect("engine"),
        );
        *bridge.engine.lock().expect("lock") = Some(Arc::clone(&engine));

        // 1. First join succeeds with join_muted = true.
        let st1 = engine
            .join_voice(Some(guild_id), ch_id)
            .await
            .expect("first join");
        assert!(st1.self_mute, "must join muted by default");
        assert_eq!(bridge.cmds.lock().expect("lock").len(), 1);
        assert!(bridge.cmds.lock().expect("lock")[0].self_mute);

        // 2. Clicking same channel again is idempotent: zero extra Op 4 commands and no timeout!
        let st2 = engine
            .join_voice(Some(guild_id), ch_id)
            .await
            .expect("idempotent rejoin");
        assert_eq!(st2.channel_id, ch_id);
        assert_eq!(
            bridge.cmds.lock().expect("lock").len(),
            1,
            "idempotent join_voice must NOT re-send Op 4"
        );

        // 3. toggle_voice on the active channel disconnects cleanly with Op 4 channel_id = None.
        let toggled = engine
            .toggle_voice(Some(guild_id), ch_id)
            .await
            .expect("toggle off");
        assert!(toggled.is_none());
        assert!(engine.status().await.expect("status").is_none());
        {
            let cmds = bridge.cmds.lock().expect("lock");
            assert_eq!(cmds.len(), 2);
            assert_eq!(cmds[1].guild_id, Some(guild_id));
            assert_eq!(cmds[1].channel_id, None);
        }

        // 4. Signaling timeout cleans up ghost Discord voice state by sending Op 4 channel_id = None.
        *bridge.endpoint.lock().expect("lock") = None;
        let err = engine.join_voice(Some(guild_id), ch_id).await;
        assert!(err.is_err(), "missing VOICE_SERVER_UPDATE must time out");
        {
            let cmds = bridge.cmds.lock().expect("lock");
            assert_eq!(cmds.len(), 4, "timed-out join must send cleanup Op 4 disconnect");
            assert_eq!(cmds[2].channel_id, Some(ch_id));
            assert_eq!(cmds[3].channel_id, None);
        }

        server.abort();
    }

    #[tokio::test]
    async fn join_muted_and_headless_capture_emit_speaking_zero() {
        use crate::rtp::{DISCOVERY_PACKET_LEN, DISCOVERY_PAYLOAD_LEN, DISCOVERY_TYPE_RESPONSE};
        use tokio::net::TcpListener;

        async fn run_case(join_muted: bool) -> Vec<u64> {
            let udp = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("udp bind");
            let udp_addr = udp.local_addr().expect("udp addr");
            let udp_task = tokio::spawn(async move {
                let mut buf = [0u8; 256];
                if let Ok((n, src)) = udp.recv_from(&mut buf).await
                    && n == DISCOVERY_PACKET_LEN
                {
                    let mut resp = [0u8; DISCOVERY_PACKET_LEN];
                    resp[0..2].copy_from_slice(&DISCOVERY_TYPE_RESPONSE.to_be_bytes());
                    resp[2..4].copy_from_slice(&DISCOVERY_PAYLOAD_LEN.to_be_bytes());
                    resp[4..8].copy_from_slice(&buf[4..8]);
                    let ip_str = src.ip().to_string();
                    resp[8..8 + ip_str.len()].copy_from_slice(ip_str.as_bytes());
                    resp[72..74].copy_from_slice(&src.port().to_be_bytes());
                    let _ = udp.send_to(&resp, src).await;
                }
            });

            let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind");
            let ws_addr = listener.local_addr().expect("addr");
            let speaking_flags = Arc::new(Mutex::new(Vec::<u64>::new()));
            let speaking_clone = Arc::clone(&speaking_flags);

            let ws_task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let ws = tokio_tungstenite::accept_async(stream)
                    .await
                    .expect("ws accept");
                let (mut tx, mut rx) = ws.split();

                let _ = tx
                    .send(WsMessage::Text(
                        json!({
                            "op": VOICE_OP_HELLO,
                            "d": { "heartbeat_interval": 30_000, "v": 8 }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;

                while let Some(Ok(WsMessage::Text(txt))) = rx.next().await {
                    let v: Value = serde_json::from_str(txt.as_str()).expect("json");
                    match v["op"].as_u64() {
                        Some(0) => {
                            let _ = tx
                                .send(WsMessage::Text(
                                    json!({
                                        "op": VOICE_OP_READY,
                                        "d": {
                                            "ssrc": 7777,
                                            "ip": udp_addr.ip().to_string(),
                                            "port": udp_addr.port(),
                                            "modes": ["aead_xchacha20_poly1305_rtpsize"]
                                        }
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                        }
                        Some(1) => {
                            let _ = tx
                                .send(WsMessage::Text(
                                    json!({
                                        "op": VOICE_OP_SESSION_DESCRIPTION,
                                        "d": {
                                            "mode": "aead_xchacha20_poly1305_rtpsize",
                                            "secret_key": vec![7u8; 32],
                                            "dave_protocol_version": 0
                                        }
                                    })
                                    .to_string()
                                    .into(),
                                ))
                                .await;
                        }
                        Some(5) => {
                            if let Some(flags) = v["d"]["speaking"].as_u64() {
                                speaking_clone.lock().expect("lock").push(flags);
                            }
                        }
                        _ => {}
                    }
                }
            });

            let cfg = VoiceEngineConfig {
                audio_mode: AudioBackendMode::Headless,
                join_muted,
                ..VoiceEngineConfig::new(Snowflake(55), "SysOp")
            };
            let engine = DiscordVoiceEngine::new(cfg, None).expect("engine");
            engine
                .connect_voice_gateway(VoiceGatewayCredentials {
                    guild_id: Some(Snowflake(10)),
                    channel_id: Snowflake(20),
                    channel_name: Some("Lounge".into()),
                    user_id: Snowflake(55),
                    session_id: Zeroizing::new("s".into()),
                    token: Zeroizing::new("t".into()),
                    endpoint: format!("ws://{ws_addr}"),
                })
                .await
                .expect("connect");

            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while tokio::time::Instant::now() < deadline {
                if speaking_flags.lock().expect("lock").len() >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            engine.leave_voice().await.expect("leave");
            ws_task.abort();
            udp_task.abort();
            let guard = speaking_flags.lock().expect("lock");
            guard.clone()
        }

        let muted_seq = run_case(true).await;
        assert_eq!(
            muted_seq,
            vec![u64::from(SPEAKING_FLAG_MICROPHONE), 0],
            "joining muted must send SPEAKING(1) to wake SFU then immediately SPEAKING(0)"
        );

        let headless_seq = run_case(false).await;
        assert_eq!(
            headless_seq,
            vec![u64::from(SPEAKING_FLAG_MICROPHONE), 0],
            "headless capture degradation must send SPEAKING(0) to clear initial indicator"
        );
    }

    #[tokio::test]
    async fn mid_call_voice_server_update_reconnects_and_reinits_dave_for_moved_channel() {
        use tokio::net::TcpListener;

        async fn spawn_identify_capture_server() -> (SocketAddr, oneshot::Receiver<Value>, JoinHandle<()>) {
            let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (id_tx, id_rx) = oneshot::channel();
            let task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.expect("accept");
                let ws = tokio_tungstenite::accept_async(stream)
                    .await
                    .expect("ws accept");
                let (mut tx, mut rx) = ws.split();
                let _ = tx
                    .send(WsMessage::Text(
                        json!({
                            "op": VOICE_OP_HELLO,
                            "d": { "heartbeat_interval": 30_000, "v": 8 }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                if let Some(Ok(WsMessage::Text(msg))) = rx.next().await {
                    let v: Value = serde_json::from_str(msg.as_str()).expect("json");
                    let _ = id_tx.send(v);
                }
                while let Some(Ok(_)) = rx.next().await {}
            });
            (addr, id_rx, task)
        }

        let (addr1, id1_rx, srv1) = spawn_identify_capture_server().await;
        let (addr2, id2_rx, srv2) = spawn_identify_capture_server().await;

        let local_uid = Snowflake(1);
        let peer_uid = Snowflake(9);
        let guild_id = Snowflake(10);
        let ch1 = Snowflake(101);
        let ch2 = Snowflake(102);

        let cfg = VoiceEngineConfig {
            audio_mode: AudioBackendMode::Headless,
            join_muted: true,
            ..VoiceEngineConfig::new(local_uid, "SysOp")
        };
        let engine = DiscordVoiceEngine::new(cfg, None).expect("engine");
        engine.set_channel_name(ch2, "Moved Room");

        // Pre-cache a remote occupant sitting in ch2 before we get moved there.
        engine
            .handle_voice_state_update(VoiceStateUpdateEvent {
                guild_id: Some(guild_id),
                channel_id: Some(ch2),
                user_id: peer_uid,
                session_id: "peer-sess".to_string(),
                self_mute: false,
                self_deaf: false,
                mute: false,
                deaf: false,
                self_stream: false,
                display_name: Some("Alice".to_string()),
            })
            .await;

        engine
            .connect_voice_gateway(VoiceGatewayCredentials {
                guild_id: Some(guild_id),
                channel_id: ch1,
                channel_name: Some("Lobby".into()),
                user_id: local_uid,
                session_id: Zeroizing::new("own-sess".into()),
                token: Zeroizing::new("tok-1".into()),
                endpoint: format!("ws://{addr1}"),
            })
            .await
            .expect("connect 1");

        let id1 = tokio::time::timeout(Duration::from_secs(3), id1_rx)
            .await
            .expect("id1 timeout")
            .expect("id1");
        assert_eq!(id1["d"]["token"], "tok-1");

        // Moderator moves local user from ch1 -> ch2.
        engine
            .handle_voice_state_update(VoiceStateUpdateEvent {
                guild_id: Some(guild_id),
                channel_id: Some(ch2),
                user_id: local_uid,
                session_id: "own-sess".to_string(),
                self_mute: true,
                self_deaf: false,
                mute: false,
                deaf: false,
                self_stream: false,
                display_name: None,
            })
            .await;

        let moved_st = engine.status().await.expect("status").expect("active");
        assert_eq!(moved_st.channel_id, ch2);
        assert_eq!(moved_st.channel_name, "Moved Room");
        assert_eq!(moved_st.connection_state, VoiceConnectionState::Reconnecting);
        assert!(
            moved_st.participants.iter().any(|p| p.user_id == peer_uid && p.display_name == "Alice"),
            "moving into ch2 must hydrate cached occupants"
        );

        // Discord clears endpoint mid-migration (endpoint: None) -> stays Reconnecting.
        engine
            .handle_voice_server_update(VoiceServerUpdateEvent {
                guild_id: Some(guild_id),
                channel_id: None,
                endpoint: None,
                token: Zeroizing::new("tok-pending".to_string()),
            })
            .await;
        let mid_st = engine.status().await.expect("status").expect("active");
        assert_eq!(mid_st.connection_state, VoiceConnectionState::Reconnecting);

        // Discord delivers new voice server endpoint -> engine reconnects & preserves mute state.
        engine
            .handle_voice_server_update(VoiceServerUpdateEvent {
                guild_id: Some(guild_id),
                channel_id: None,
                endpoint: Some(format!("ws://{addr2}")),
                token: Zeroizing::new("tok-2".to_string()),
            })
            .await;

        let id2 = tokio::time::timeout(Duration::from_secs(3), id2_rx)
            .await
            .expect("id2 timeout")
            .expect("id2");
        assert_eq!(id2["d"]["token"], "tok-2");
        assert_eq!(id2["d"]["server_id"], guild_id.to_string());

        let reconnected_st = engine.status().await.expect("status").expect("active");
        assert_eq!(reconnected_st.channel_id, ch2);
        assert!(reconnected_st.self_mute, "reconnect must preserve self_mute");

        engine.leave_voice().await.expect("leave");
        srv1.abort();
        srv2.abort();
    }

    #[tokio::test]
    async fn local_server_mute_deaf_sync_and_channel_scoped_call_delete() {
        let local_uid = Snowflake(100);
        let guild_id = Snowflake(200);
        let ch_id = Snowflake(300);
        let cfg = VoiceEngineConfig {
            audio_mode: AudioBackendMode::Headless,
            ..VoiceEngineConfig::new(local_uid, "SysOp")
        };
        let engine = DiscordVoiceEngine::new(cfg, None).expect("engine");
        let mut rx = engine.subscribe_voice_events();

        // Populate an active voice session to verify local server_mute / server_deaf sync.
        {
            let mut g = engine.state.lock().expect("lock");
            g.active_session_id = Some("sess-x".to_string());
            g.status = Some(VoiceChannelStatus {
                guild_id: Some(guild_id),
                channel_id: ch_id,
                channel_name: "General".to_string(),
                connection_state: VoiceConnectionState::Connected,
                self_mute: false,
                self_deaf: false,
                push_to_talk: false,
                ping_ms: Some(10),
                packet_loss_pct: 0.0,
                dave: DaveSessionState::Disabled,
                call_ring: CallRingState::Idle,
                participants: Vec::new(),
            });
            g.ensure_participant(local_uid, Some("SysOp".to_string()));
        }

        engine
            .handle_voice_state_update(VoiceStateUpdateEvent {
                guild_id: Some(guild_id),
                channel_id: Some(ch_id),
                user_id: local_uid,
                session_id: "sess-x".to_string(),
                self_mute: false,
                self_deaf: false,
                mute: true,
                deaf: true,
                self_stream: true,
                display_name: None,
            })
            .await;

        let st = engine.status().await.expect("status").expect("active");
        let me = st
            .participants
            .iter()
            .find(|p| p.user_id == local_uid)
            .expect("local participant");
        assert!(me.server_mute, "local server_mute must sync into participant");
        assert!(me.server_deaf, "local server_deaf must sync into participant");
        assert!(me.streaming, "local self_stream must sync into participant");
        assert!(!me.speaking);

        // Drain the VoiceStatusUpdated event before testing call_delete channel scoping.
        let _ = rx.recv().await;

        // Incoming ring on DM channel 500.
        engine.handle_call_update(CallUpdateEvent {
            channel_id: Snowflake(500),
            ringing: vec![local_uid],
            connected_user_ids: vec![Snowflake(201)],
            caller_id: Some(Snowflake(201)),
            caller_name: Some("Alice".into()),
            is_group_dm: false,
        });
        let ring_ev = rx.recv().await.expect("ring event");
        assert!(matches!(
            ring_ev,
            BackendEvent::CallRingUpdated(CallRingState::IncomingRinging {
                channel_id: Snowflake(500),
                ..
            })
        ));

        // Unrelated DM channel 999 CALL_DELETE must NOT emit CallRingUpdated(Idle).
        engine.handle_call_delete(Snowflake(999));
        assert!(
            rx.try_recv().is_err(),
            "CALL_DELETE for unrelated channel must not emit Idle or clobber active ring"
        );

        // Deleting channel 500 DOES emit CallRingUpdated(Idle).
        engine.handle_call_delete(Snowflake(500));
        let idle_ev = rx.recv().await.expect("idle event");
        assert!(matches!(
            idle_ev,
            BackendEvent::CallRingUpdated(CallRingState::Idle)
        ));
    }
}
