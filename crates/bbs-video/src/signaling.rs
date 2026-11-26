//! Discord Go Live stream keys, Main Gateway commands/dispatches, and Stream RTC WebSocket signaling.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bbs_core::{CoreError, CoreResult, DaveSessionState, Snowflake, StreamQuality};
use davey::{CommitWelcome, DAVE_PROTOCOL_VERSION, ProposalsOperationType};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, trace, warn};
use zeroize::Zeroizing;

use crate::codec::{SdpCodecSpec, VideoCodec};
use crate::dave::DaveVideoContext;

/// Discord Main Gateway opcodes used for Go Live screenshare coordination.
pub mod gateway_op {
    /// Start a Go Live stream (`STREAM_CREATE`).
    pub const STREAM_CREATE: u8 = 18;
    /// Stop/delete a Go Live stream (`STREAM_DELETE`).
    pub const STREAM_DELETE: u8 = 19;
    /// Request to watch an existing Go Live stream (`STREAM_WATCH`).
    pub const STREAM_WATCH: u8 = 20;
    /// Keep a Go Live stream session alive (`STREAM_PING`).
    pub const STREAM_PING: u8 = 21;
    /// Pause or resume an outbound Go Live stream (`STREAM_SET_PAUSED`).
    pub const STREAM_SET_PAUSED: u8 = 22;
}

/// Discord Stream RTC / Voice Gateway v8 opcodes (including DAVE E2EE opcodes).
pub mod rtc_op {
    /// Identify against the stream RTC server.
    pub const IDENTIFY: u8 = 0;
    /// Negotiate WebRTC SDP offer and video/audio codecs.
    pub const SELECT_PROTOCOL: u8 = 1;
    /// Initial RTC readiness snapshot with allocated SSRCs and stream descriptors.
    pub const READY: u8 = 2;
    /// Client heartbeat keepalive.
    pub const HEARTBEAT: u8 = 3;
    /// Server WebRTC SDP answer and selected video codec / DAVE protocol version.
    pub const SESSION_DESCRIPTION: u8 = 4;
    /// Speaking indicator (audio SSRC).
    pub const SPEAKING: u8 = 5;
    /// Server acknowledgement of client heartbeat.
    pub const HEARTBEAT_ACK: u8 = 6;
    /// Resume dropped Stream RTC session.
    pub const RESUME: u8 = 7;
    /// Initial handshake frame supplying heartbeat interval.
    pub const HELLO: u8 = 8;
    /// Stream RTC resume confirmed.
    pub const RESUMED: u8 = 9;
    /// Remote participants connected to the stream RTC room.
    pub const CLIENTS_CONNECT: u8 = 11;
    /// Video/RTX SSRC announcement (client -> server and server -> client).
    pub const VIDEO: u8 = 12;
    /// Remote participant disconnected from the stream RTC room.
    pub const CLIENT_DISCONNECT: u8 = 13;
    /// Viewer quality/simulcast sink preferences.
    pub const MEDIA_SINK_WANTS: u8 = 15;
    /// Prepare upcoming DAVE MLS key transition (JSON).
    pub const DAVE_PREPARE_TRANSITION: u8 = 21;
    /// Execute previously prepared DAVE MLS key transition (JSON).
    pub const DAVE_EXECUTE_TRANSITION: u8 = 22;
    /// Client confirmation that a DAVE transition is ready (JSON).
    pub const DAVE_TRANSITION_READY: u8 = 23;
    /// Prepare DAVE MLS epoch reset/re-init (JSON).
    pub const DAVE_PREPARE_EPOCH: u8 = 24;
    /// Server-delivered MLS external sender package (Binary).
    pub const DAVE_MLS_EXTERNAL_SENDER: u8 = 25;
    /// Client-uploaded MLS KeyPackage (Binary).
    pub const DAVE_MLS_KEY_PACKAGE: u8 = 26;
    /// Server-delivered MLS proposals (Binary).
    pub const DAVE_MLS_PROPOSALS: u8 = 27;
    /// Client-uploaded MLS Commit + optional Welcome (Binary).
    pub const DAVE_MLS_COMMIT_WELCOME: u8 = 28;
    /// Server-announced MLS Commit transition (Binary).
    pub const DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION: u8 = 29;
    /// Server-delivered MLS Welcome transition (Binary).
    pub const DAVE_MLS_WELCOME: u8 = 30;
    /// Client notification that a DAVE commit or welcome was rejected (JSON).
    pub const DAVE_MLS_INVALID_COMMIT_WELCOME: u8 = 31;
}

/// Redacted wrapper around a Discord Stream RTC authentication token.
///
/// Zeroized on drop and always renders as `"[REDACTED]"` in `Debug` output so stream tokens
/// are never leaked to logs or diagnostics.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedStreamToken(Zeroizing<String>);

impl RedactedStreamToken {
    /// Wrap a stream RTC token string in a zeroizing buffer.
    #[must_use]
    pub fn new(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    /// Borrow the raw token strictly for constructing the `IDENTIFY` WebSocket frame.
    #[must_use]
    pub fn expose_for_identify(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for RedactedStreamToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedactedStreamToken([REDACTED])")
    }
}

/// Canonical Discord Go Live stream key covering both Guild Voice and DM / Group DM calls.
///
/// Formats:
/// - Guild Voice: `guild:<guild_id>:<channel_id>:<user_id>`
/// - DM / Group DM Call: `call:<channel_id>:<user_id>`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamKey {
    /// Screenshare hosted inside a Guild Voice or Stage channel.
    Guild {
        /// Owning guild Snowflake.
        guild_id: Snowflake,
        /// Voice channel Snowflake.
        channel_id: Snowflake,
        /// Broadcaster user Snowflake.
        user_id: Snowflake,
    },
    /// Screenshare hosted inside a 1:1 DM or Group DM call.
    Call {
        /// DM or Group DM channel Snowflake.
        channel_id: Snowflake,
        /// Broadcaster user Snowflake.
        user_id: Snowflake,
    },
}

impl StreamKey {
    /// Construct a canonical [`StreamKey`] from optional guild ID, channel ID, and broadcaster user ID.
    #[must_use]
    pub const fn from_ids(
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> Self {
        match guild_id {
            Some(guild_id) => Self::Guild {
                guild_id,
                channel_id,
                user_id,
            },
            None => Self::Call {
                channel_id,
                user_id,
            },
        }
    }

    /// Parse a canonical Go Live stream key string (`guild:<g>:<c>:<u>` or `call:<c>:<u>`).
    pub fn parse(raw: &str) -> CoreResult<Self> {
        let trimmed = raw.trim();
        let parts: Vec<&str> = trimmed.split(':').collect();
        match parts.as_slice() {
            ["guild", guild, channel, user] => Ok(Self::Guild {
                guild_id: Snowflake::from_str(guild)?,
                channel_id: Snowflake::from_str(channel)?,
                user_id: Snowflake::from_str(user)?,
            }),
            ["call", channel, user] => Ok(Self::Call {
                channel_id: Snowflake::from_str(channel)?,
                user_id: Snowflake::from_str(user)?,
            }),
            _ => Err(CoreError::Protocol(format!(
                "invalid Go Live stream key `{trimmed}` (expected guild:<g>:<c>:<u> or call:<c>:<u>)"
            ))),
        }
    }

    /// Return the owning guild Snowflake (`None` for DM / Group DM calls).
    #[must_use]
    pub const fn guild_id(self) -> Option<Snowflake> {
        match self {
            Self::Guild { guild_id, .. } => Some(guild_id),
            Self::Call { .. } => None,
        }
    }

    /// Return the voice or call channel Snowflake.
    #[must_use]
    pub const fn channel_id(self) -> Snowflake {
        match self {
            Self::Guild { channel_id, .. } | Self::Call { channel_id, .. } => channel_id,
        }
    }

    /// Return the broadcaster's user Snowflake.
    #[must_use]
    pub const fn user_id(self) -> Snowflake {
        match self {
            Self::Guild { user_id, .. } | Self::Call { user_id, .. } => user_id,
        }
    }

    /// Return `true` if this stream belongs to a 1:1 DM or Group DM call.
    #[must_use]
    pub const fn is_call(self) -> bool {
        matches!(self, Self::Call { .. })
    }

    /// Discord Go Live `type` string (`"guild"` or `"call"`).
    #[must_use]
    pub const fn discord_stream_type(self) -> &'static str {
        match self {
            Self::Guild { .. } => "guild",
            Self::Call { .. } => "call",
        }
    }

    /// Render the canonical stream key string.
    #[must_use]
    pub fn to_canonical_string(self) -> String {
        self.to_string()
    }
}

impl fmt::Display for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Guild {
                guild_id,
                channel_id,
                user_id,
            } => write!(f, "guild:{guild_id}:{channel_id}:{user_id}"),
            Self::Call {
                channel_id,
                user_id,
            } => write!(f, "call:{channel_id}:{user_id}"),
        }
    }
}

impl FromStr for StreamKey {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for StreamKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for StreamKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Outbound Discord Main Gateway command emitted by `bbs-video` for Go Live coordination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GatewayStreamCommand {
    /// Op 18 (`STREAM_CREATE`): start broadcasting in a guild voice or DM/Group DM call channel.
    Create {
        /// Canonical target stream key.
        stream_key: StreamKey,
        /// Optional preferred voice region (e.g., `"us-central"`).
        preferred_region: Option<String>,
    },
    /// Op 20 (`STREAM_WATCH`): subscribe to watch an active remote Go Live stream.
    Watch {
        /// Canonical target stream key.
        stream_key: StreamKey,
    },
    /// Op 22 (`STREAM_SET_PAUSED`): pause or resume the active stream.
    SetPaused {
        /// Canonical target stream key.
        stream_key: StreamKey,
        /// `true` to pause video transmission, `false` to resume.
        paused: bool,
    },
    /// Op 19 (`STREAM_DELETE`): end a broadcast or stop watching a stream.
    Delete {
        /// Canonical target stream key.
        stream_key: StreamKey,
    },
    /// Op 21 (`STREAM_PING`): periodic keepalive for an active stream session.
    Ping {
        /// Canonical target stream key.
        stream_key: StreamKey,
    },
}

impl GatewayStreamCommand {
    /// Return the Discord Main Gateway opcode for this command.
    #[must_use]
    pub const fn opcode(&self) -> u8 {
        match self {
            Self::Create { .. } => gateway_op::STREAM_CREATE,
            Self::Delete { .. } => gateway_op::STREAM_DELETE,
            Self::Watch { .. } => gateway_op::STREAM_WATCH,
            Self::Ping { .. } => gateway_op::STREAM_PING,
            Self::SetPaused { .. } => gateway_op::STREAM_SET_PAUSED,
        }
    }

    /// Encode this command as a complete Discord Main Gateway JSON envelope `{ "op": ..., "d": ... }`.
    #[must_use]
    pub fn to_gateway_payload(&self) -> Value {
        match self {
            Self::Create {
                stream_key,
                preferred_region,
            } => json!({
                "op": gateway_op::STREAM_CREATE,
                "d": {
                    "type": stream_key.discord_stream_type(),
                    "guild_id": stream_key.guild_id().map(|g| g.to_string()),
                    "channel_id": stream_key.channel_id().to_string(),
                    "preferred_region": preferred_region,
                }
            }),
            Self::Watch { stream_key } => json!({
                "op": gateway_op::STREAM_WATCH,
                "d": {
                    "stream_key": stream_key.to_canonical_string(),
                }
            }),
            Self::SetPaused { stream_key, paused } => json!({
                "op": gateway_op::STREAM_SET_PAUSED,
                "d": {
                    "stream_key": stream_key.to_canonical_string(),
                    "paused": paused,
                }
            }),
            Self::Delete { stream_key } => json!({
                "op": gateway_op::STREAM_DELETE,
                "d": {
                    "stream_key": stream_key.to_canonical_string(),
                }
            }),
            Self::Ping { stream_key } => json!({
                "op": gateway_op::STREAM_PING,
                "d": {
                    "stream_key": stream_key.to_canonical_string(),
                }
            }),
        }
    }
}

/// Inbound Discord Main Gateway dispatch events related to Go Live streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayStreamEvent {
    /// `STREAM_CREATE`: stream session created on Discord's SFU.
    StreamCreate {
        /// Canonical stream key.
        stream_key: StreamKey,
        /// Allocated stream RTC server ID (`server_id` for Stream RTC `IDENTIFY`).
        rtc_server_id: Snowflake,
        /// Optional stream RTC channel ID.
        rtc_channel_id: Option<Snowflake>,
        /// Assigned voice region.
        region: Option<String>,
        /// Viewer user Snowflakes currently watching.
        viewer_ids: Vec<Snowflake>,
        /// Whether the stream is currently paused.
        paused: bool,
    },
    /// `STREAM_SERVER_UPDATE`: RTC WebSocket endpoint and credentials allocated.
    StreamServerUpdate {
        /// Canonical stream key.
        stream_key: StreamKey,
        /// Stream RTC WebSocket endpoint hostname/port (e.g. `"us-central123.discord.media:443"`).
        endpoint: String,
        /// Ephemeral Stream RTC token (redacted and zeroized on drop).
        token: RedactedStreamToken,
    },
    /// `STREAM_UPDATE`: viewer roster or pause state changed.
    StreamUpdate {
        /// Canonical stream key.
        stream_key: StreamKey,
        /// Updated voice region.
        region: Option<String>,
        /// Updated viewer user Snowflakes.
        viewer_ids: Vec<Snowflake>,
        /// Updated paused flag.
        paused: bool,
    },
    /// `STREAM_DELETE`: Go Live stream terminated or became unavailable.
    StreamDelete {
        /// Canonical stream key.
        stream_key: StreamKey,
        /// Optional server reason (e.g., `"user_requested"`, `"stream_full"`).
        reason: Option<String>,
        /// Whether the stream is temporarily unavailable.
        unavailable: bool,
    },
}

/// Parse a Discord Main Gateway dispatch event (`t` + `d`) into a [`GatewayStreamEvent`] when
/// applicable. Returns `Ok(None)` for unrelated Gateway dispatches.
pub fn parse_gateway_dispatch(
    event_name: &str,
    data: &Value,
) -> CoreResult<Option<GatewayStreamEvent>> {
    match event_name {
        "STREAM_CREATE" => {
            let stream_key = parse_required_stream_key(data)?;
            let rtc_server_id = parse_required_snowflake(data, "rtc_server_id")?;
            let rtc_channel_id = parse_optional_snowflake(data, "rtc_channel_id")?;
            let region = data
                .get("region")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let viewer_ids = parse_snowflake_array(data, "viewer_ids")?;
            let paused = data.get("paused").and_then(Value::as_bool).unwrap_or(false);
            Ok(Some(GatewayStreamEvent::StreamCreate {
                stream_key,
                rtc_server_id,
                rtc_channel_id,
                region,
                viewer_ids,
                paused,
            }))
        }
        "STREAM_SERVER_UPDATE" => {
            let stream_key = parse_required_stream_key(data)?;
            let endpoint = data
                .get("endpoint")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CoreError::Protocol("STREAM_SERVER_UPDATE missing endpoint".to_string())
                })?
                .to_string();
            let token = data
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    CoreError::Protocol("STREAM_SERVER_UPDATE missing token".to_string())
                })?
                .to_string();
            Ok(Some(GatewayStreamEvent::StreamServerUpdate {
                stream_key,
                endpoint,
                token: RedactedStreamToken::new(token),
            }))
        }
        "STREAM_UPDATE" => {
            let stream_key = parse_required_stream_key(data)?;
            let region = data
                .get("region")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let viewer_ids = parse_snowflake_array(data, "viewer_ids")?;
            let paused = data.get("paused").and_then(Value::as_bool).unwrap_or(false);
            Ok(Some(GatewayStreamEvent::StreamUpdate {
                stream_key,
                region,
                viewer_ids,
                paused,
            }))
        }
        "STREAM_DELETE" => {
            let stream_key = parse_required_stream_key(data)?;
            let reason = data
                .get("reason")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let unavailable = data
                .get("unavailable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(Some(GatewayStreamEvent::StreamDelete {
                stream_key,
                reason,
                unavailable,
            }))
        }
        _ => Ok(None),
    }
}

/// Allocated or remote SSRC mapping for a Go Live video stream (video + RTX retransmission).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSsrcMap {
    /// Associated stream audio SSRC (`0` when video-only).
    pub audio_ssrc: u32,
    /// Primary video RTP SSRC.
    pub video_ssrc: u32,
    /// Associated RTX retransmission SSRC (`video_ssrc + 1` on Discord SFUs).
    pub rtx_ssrc: u32,
    /// Simulcast / stream RID label (typically `"100"`).
    pub rid: String,
    /// Target stream quality tier (`100` = full quality).
    pub quality: u32,
    /// Whether this video stream layer is active.
    pub active: bool,
}

impl Default for StreamSsrcMap {
    fn default() -> Self {
        Self {
            audio_ssrc: 0,
            video_ssrc: 0,
            rtx_ssrc: 0,
            rid: "100".to_string(),
            quality: 100,
            active: true,
        }
    }
}

/// Parameters required to dial a Discord Stream RTC WebSocket (`wss://<endpoint>/?v=8`).
#[derive(Clone)]
pub struct StreamRtcConfig {
    /// Canonical Go Live stream key.
    pub stream_key: StreamKey,
    /// Stream RTC server ID from `STREAM_CREATE.rtc_server_id`.
    pub rtc_server_id: Snowflake,
    /// Signed-in local user Snowflake.
    pub user_id: Snowflake,
    /// Active Main Gateway session ID.
    pub session_id: String,
    /// Stream RTC endpoint from `STREAM_SERVER_UPDATE.endpoint`.
    pub endpoint: String,
    /// Stream RTC token from `STREAM_SERVER_UPDATE.token` (redacted).
    pub token: RedactedStreamToken,
    /// Ordered video codecs to advertise in `SELECT_PROTOCOL`.
    pub codecs: Vec<SdpCodecSpec>,
    /// Local WebRTC SDP offer generated by `webrtcbin` (optional; synthesized if empty).
    pub local_sdp_offer: String,
    /// Desired outbound stream quality (used when publishing Op 12 `VIDEO`).
    pub broadcast_quality: Option<StreamQuality>,
    /// Whether to advertise `max_dave_protocol_version = 1`.
    pub enable_dave: bool,
}

impl fmt::Debug for StreamRtcConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamRtcConfig")
            .field("stream_key", &self.stream_key)
            .field("rtc_server_id", &self.rtc_server_id)
            .field("user_id", &self.user_id)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token)
            .field("codecs", &self.codecs)
            .field("enable_dave", &self.enable_dave)
            .finish()
    }
}

/// Events emitted by [`StreamRtcClient`] for pipeline synchronization and UI state updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamRtcEvent {
    /// Received Op 2 `READY` with allocated local SSRCs.
    Ready {
        /// Local audio/video/rtx SSRCs assigned by the SFU.
        ssrcs: StreamSsrcMap,
    },
    /// Received Op 4 `SESSION_DESCRIPTION` with remote WebRTC SDP answer & negotiated codec.
    SessionDescription {
        /// Remote SDP answer string to feed into GStreamer `webrtcbin`.
        remote_sdp: String,
        /// Negotiated video codec (`H264`, `Vp8`, `Vp9`).
        video_codec: VideoCodec,
        /// Optional server media session identifier.
        media_session_id: Option<String>,
        /// Negotiated DAVE protocol version (`0` = disabled, `1` = DAVE v1 MLS).
        dave_protocol_version: u16,
    },
    /// Received Op 12 `VIDEO` mapping a remote broadcaster's video & RTX SSRCs.
    RemoteVideoSsrc {
        /// Remote broadcaster user Snowflake.
        user_id: Snowflake,
        /// Remote video/RTX SSRC mapping.
        ssrcs: StreamSsrcMap,
    },
    /// DAVE MLS session state updated (handshake progressed or epoch transitioned).
    DaveStateChanged(DaveSessionState),
    /// Stream RTC WebSocket disconnected cleanly or with an error message.
    Disconnected {
        /// Human-readable reason (never includes tokens).
        reason: String,
    },
}

/// Decoded server -> client DAVE binary frame from the Stream RTC WebSocket.
///
/// Wire format (Voice Gateway v8): `[seq: u16 big-endian][opcode: u8][body...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerDaveFrame {
    /// Op 25: MLS ExternalSender serialized package.
    ExternalSender {
        /// Server sequence number (`seq_ack`).
        seq: u16,
        /// Serialized `ExternalSender`.
        payload: Vec<u8>,
    },
    /// Op 27: MLS Proposals append/revoke list.
    Proposals {
        /// Server sequence number (`seq_ack`).
        seq: u16,
        /// Operation type (`APPEND = 0`, `REVOKE = 1`).
        operation_type: ProposalsOperationType,
        /// TLS-serialized `VLBytes` proposals payload.
        proposals: Vec<u8>,
    },
    /// Op 29: MLS Commit transition announcement.
    AnnounceCommitTransition {
        /// Server sequence number (`seq_ack`).
        seq: u16,
        /// Transition ID (`u16` big-endian).
        transition_id: u16,
        /// Serialized MLS Commit message.
        commit: Vec<u8>,
    },
    /// Op 30: MLS Welcome transition payload.
    Welcome {
        /// Server sequence number (`seq_ack`).
        seq: u16,
        /// Transition ID (`u16` big-endian).
        transition_id: u16,
        /// Serialized MLS Welcome message.
        welcome: Vec<u8>,
    },
}

/// Parse a server -> client Voice Gateway v8 DAVE binary frame (`[seq: u16 BE][op: u8][body..]`).
pub fn parse_server_dave_binary_frame(bytes: &[u8]) -> CoreResult<ServerDaveFrame> {
    if bytes.len() < 3 {
        return Err(CoreError::Protocol(format!(
            "DAVE binary frame too short ({} bytes, expected >= 3)",
            bytes.len()
        )));
    }
    let seq = u16::from_be_bytes([bytes[0], bytes[1]]);
    let op = bytes[2];
    let body = &bytes[3..];

    match op {
        rtc_op::DAVE_MLS_EXTERNAL_SENDER => Ok(ServerDaveFrame::ExternalSender {
            seq,
            payload: body.to_vec(),
        }),
        rtc_op::DAVE_MLS_PROPOSALS => {
            if body.is_empty() {
                return Err(CoreError::Protocol(
                    "DAVE_MLS_PROPOSALS missing operation_type byte".to_string(),
                ));
            }
            let operation_type = match body[0] {
                0 => ProposalsOperationType::APPEND,
                1 => ProposalsOperationType::REVOKE,
                other => {
                    return Err(CoreError::Protocol(format!(
                        "invalid DAVE_MLS_PROPOSALS operation_type {other}"
                    )));
                }
            };
            Ok(ServerDaveFrame::Proposals {
                seq,
                operation_type,
                proposals: body[1..].to_vec(),
            })
        }
        rtc_op::DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION => {
            if body.len() < 2 {
                return Err(CoreError::Protocol(
                    "DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION missing transition_id".to_string(),
                ));
            }
            let transition_id = u16::from_be_bytes([body[0], body[1]]);
            Ok(ServerDaveFrame::AnnounceCommitTransition {
                seq,
                transition_id,
                commit: body[2..].to_vec(),
            })
        }
        rtc_op::DAVE_MLS_WELCOME => {
            if body.len() < 2 {
                return Err(CoreError::Protocol(
                    "DAVE_MLS_WELCOME missing transition_id".to_string(),
                ));
            }
            let transition_id = u16::from_be_bytes([body[0], body[1]]);
            Ok(ServerDaveFrame::Welcome {
                seq,
                transition_id,
                welcome: body[2..].to_vec(),
            })
        }
        other => Err(CoreError::Protocol(format!(
            "unexpected server DAVE binary opcode {other}"
        ))),
    }
}

/// Encode a client -> server DAVE binary frame (`[op: u8][body...]`).
#[must_use]
pub fn encode_client_dave_binary_frame(opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(opcode);
    out.extend_from_slice(body);
    out
}

/// Encode a client -> server Op 28 `DAVE_MLS_COMMIT_WELCOME` binary frame.
#[must_use]
pub fn encode_dave_commit_welcome(cw: &CommitWelcome) -> Vec<u8> {
    let welcome_len = cw.welcome.as_ref().map_or(0, Vec::len);
    let mut out = Vec::with_capacity(1 + cw.commit.len() + welcome_len);
    out.push(rtc_op::DAVE_MLS_COMMIT_WELCOME);
    out.extend_from_slice(&cw.commit);
    if let Some(welcome) = &cw.welcome {
        out.extend_from_slice(welcome);
    }
    out
}

/// Build the Stream RTC Op 0 `IDENTIFY` JSON payload.
#[must_use]
pub fn build_identify_payload(config: &StreamRtcConfig) -> Value {
    let max_dave = if config.enable_dave {
        DAVE_PROTOCOL_VERSION
    } else {
        0
    };
    json!({
        "op": rtc_op::IDENTIFY,
        "d": {
            "server_id": config.rtc_server_id.to_string(),
            "user_id": config.user_id.to_string(),
            "session_id": config.session_id,
            "token": config.token.expose_for_identify(),
            "video": true,
            "max_dave_protocol_version": max_dave,
            "streams": [
                {
                    "type": "video",
                    "rid": "100",
                    "quality": 100
                }
            ]
        }
    })
}

/// Build the Stream RTC Op 1 `SELECT_PROTOCOL` payload for WebRTC SDP + codec negotiation.
#[must_use]
pub fn build_select_protocol_webrtc_payload(
    local_sdp_offer: &str,
    codecs: &[SdpCodecSpec],
) -> Value {
    let mut codec_entries: Vec<Value> = codecs
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "type": spec.media_type,
                "priority": spec.priority,
                "payload_type": spec.payload_type,
                "rtx_payload_type": spec.rtx_payload_type,
                "encode": spec.encode,
                "decode": spec.decode,
            })
        })
        .collect();

    // Include Opus audio codec alongside video codecs for Go Live stream audio compatibility.
    codec_entries.push(json!({
        "name": "opus",
        "type": "audio",
        "priority": 1000,
        "payload_type": 111,
        "encode": true,
        "decode": true,
    }));

    json!({
        "op": rtc_op::SELECT_PROTOCOL,
        "d": {
            "protocol": "webrtc",
            "data": local_sdp_offer,
            "sdp": local_sdp_offer,
            "codecs": codec_entries,
        }
    })
}

/// Build the Stream RTC Op 12 `VIDEO` SSRC announcement payload for broadcasting.
#[must_use]
pub fn build_video_announcement_payload(
    ssrcs: &StreamSsrcMap,
    quality: Option<StreamQuality>,
) -> Value {
    let stream_entry = if let Some(q) = quality {
        json!({
            "type": "video",
            "rid": ssrcs.rid,
            "ssrc": ssrcs.video_ssrc,
            "rtx_ssrc": ssrcs.rtx_ssrc,
            "active": ssrcs.active,
            "quality": ssrcs.quality,
            "max_bitrate": q.max_bitrate_kbps.saturating_mul(1_000),
            "max_framerate": q.fps,
            "max_resolution": {
                "type": "fixed",
                "width": q.width,
                "height": q.height,
            }
        })
    } else {
        json!({
            "type": "video",
            "rid": ssrcs.rid,
            "ssrc": ssrcs.video_ssrc,
            "rtx_ssrc": ssrcs.rtx_ssrc,
            "active": ssrcs.active,
            "quality": ssrcs.quality,
        })
    };

    json!({
        "op": rtc_op::VIDEO,
        "d": {
            "audio_ssrc": ssrcs.audio_ssrc,
            "video_ssrc": ssrcs.video_ssrc,
            "rtx_ssrc": ssrcs.rtx_ssrc,
            "streams": [stream_entry],
        }
    })
}

/// Build the Stream RTC Op 15 `MEDIA_SINK_WANTS` payload requesting quality for `video_ssrc`.
#[must_use]
pub fn build_media_sink_wants_payload(video_ssrc: u32, quality: u32) -> Value {
    let mut wants = serde_json::Map::new();
    wants.insert("any".to_string(), json!(quality));
    if video_ssrc != 0 {
        wants.insert(video_ssrc.to_string(), json!(quality));
    }
    json!({
        "op": rtc_op::MEDIA_SINK_WANTS,
        "d": Value::Object(wants),
    })
}

/// Build a normalized `wss://<endpoint>/?v=8` URL from Discord's `STREAM_SERVER_UPDATE.endpoint`.
#[must_use]
pub fn normalize_rtc_endpoint_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let base = if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    };
    if base.contains('?') {
        base
    } else {
        format!("{base}/?v=8")
    }
}

/// Live asynchronous Stream RTC WebSocket client coordinating WebRTC SDP, SSRC mapping, and DAVE MLS.
pub struct StreamRtcClient {
    config: StreamRtcConfig,
    dave: DaveVideoContext,
    events_tx: broadcast::Sender<StreamRtcEvent>,
    outbound_tx: mpsc::Sender<WsMessage>,
    ssrcs: Arc<RwLock<StreamSsrcMap>>,
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl fmt::Debug for StreamRtcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamRtcClient")
            .field("config", &self.config)
            .field("dave", &self.dave)
            .finish()
    }
}

impl StreamRtcClient {
    /// Dial the Stream RTC WebSocket and spawn its non-blocking protocol/heartbeat/DAVE state machine.
    pub async fn connect(config: StreamRtcConfig, dave: DaveVideoContext) -> CoreResult<Self> {
        let url = normalize_rtc_endpoint_url(&config.endpoint);
        info!(
            stream_key = %config.stream_key,
            endpoint = %config.endpoint,
            "connecting to Discord Stream RTC WebSocket"
        );

        let (ws_stream, _response) = connect_async(&url).await.map_err(|err| {
            CoreError::Network(format!("failed to connect Stream RTC WebSocket: {err}"))
        })?;

        let (events_tx, _) = broadcast::channel(64);
        let (outbound_tx, outbound_rx) = mpsc::channel(64);
        let ssrcs = Arc::new(RwLock::new(StreamSsrcMap::default()));

        let handle = tokio::spawn(run_stream_rtc_loop(
            ws_stream,
            config.clone(),
            dave.clone(),
            events_tx.clone(),
            outbound_tx.clone(),
            outbound_rx,
            Arc::clone(&ssrcs),
        ));

        Ok(Self {
            config,
            dave,
            events_tx,
            outbound_tx,
            ssrcs,
            task_handle: Some(handle),
        })
    }

    /// Subscribe to [`StreamRtcEvent`] notifications (READY, SDP answer, SSRC updates, DAVE state).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StreamRtcEvent> {
        self.events_tx.subscribe()
    }

    /// Return the latest allocated local SSRC map.
    pub async fn local_ssrcs(&self) -> StreamSsrcMap {
        self.ssrcs.read().await.clone()
    }

    /// Send an Op 12 `VIDEO` SSRC announcement over the live Stream RTC WebSocket.
    pub async fn announce_video(
        &self,
        ssrcs: &StreamSsrcMap,
        quality: Option<StreamQuality>,
    ) -> CoreResult<()> {
        let payload = build_video_announcement_payload(ssrcs, quality);
        self.send_json(&payload).await
    }

    /// Send an Op 15 `MEDIA_SINK_WANTS` quality request for a watched remote video SSRC.
    pub async fn request_media_sink_wants(&self, video_ssrc: u32, quality: u32) -> CoreResult<()> {
        let payload = build_media_sink_wants_payload(video_ssrc, quality);
        self.send_json(&payload).await
    }

    /// Send a raw JSON protocol message on the Stream RTC WebSocket.
    pub async fn send_json(&self, payload: &Value) -> CoreResult<()> {
        let serialized = serde_json::to_string(payload)
            .map_err(|err| CoreError::Protocol(format!("failed to serialize RTC JSON: {err}")))?;
        self.outbound_tx
            .send(WsMessage::text(serialized))
            .await
            .map_err(|_| CoreError::Network("Stream RTC WebSocket writer closed".to_string()))
    }

    /// Gracefully shut down the Stream RTC WebSocket task.
    pub async fn disconnect(&mut self) {
        let _ = self.outbound_tx.send(WsMessage::Close(None)).await;
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl Drop for StreamRtcClient {
    fn drop(&mut self) {
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }
}

async fn run_stream_rtc_loop<S>(
    ws_stream: S,
    config: StreamRtcConfig,
    dave: DaveVideoContext,
    events_tx: broadcast::Sender<StreamRtcEvent>,
    outbound_tx: mpsc::Sender<WsMessage>,
    mut outbound_rx: mpsc::Receiver<WsMessage>,
    ssrcs_store: Arc<RwLock<StreamSsrcMap>>,
) where
    S: futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin
        + Send
        + 'static,
{
    let (mut ws_sink, mut ws_reader) = ws_stream.split();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if let Err(err) = ws_sink.send(msg).await {
                debug!(error = %err, "Stream RTC WebSocket sink closed");
                break;
            }
        }
    });

    // Send Op 0 IDENTIFY immediately after connecting.
    let identify_json = build_identify_payload(&config);
    if let Ok(text) = serde_json::to_string(&identify_json) {
        let _ = outbound_tx.send(WsMessage::text(text)).await;
    }

    let mut heartbeat_task: Option<tokio::task::JoinHandle<()>> = None;
    let seq_ack = Arc::new(std::sync::atomic::AtomicI64::new(-1));
    let awaiting_heartbeat_ack = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (control_tx, mut control_rx) = mpsc::channel::<String>(4);
    let mut known_users: HashSet<u64> =
        HashSet::from([config.user_id.as_u64(), config.stream_key.user_id().as_u64()]);
    let mut received_clients_connect = false;
    let mut negotiated_dave_version: u16 = if config.enable_dave {
        DAVE_PROTOCOL_VERSION
    } else {
        0
    };

    let disconnect_reason = loop {
        let next_msg = tokio::select! {
            biased;
            Some(reason) = control_rx.recv() => break reason,
            msg = ws_reader.next() => msg,
        };
        match next_msg {
            Some(Ok(WsMessage::Text(text))) => {
                let parsed: Value = match serde_json::from_str(text.as_str()) {
                    Ok(v) => v,
                    Err(err) => {
                        warn!(error = %err, "ignoring malformed JSON from Stream RTC WS");
                        continue;
                    }
                };

                if let Some(seq) = parsed.get("seq").and_then(Value::as_i64) {
                    seq_ack.store(seq, std::sync::atomic::Ordering::Relaxed);
                }

                let Some(op) = parsed.get("op").and_then(Value::as_u64).map(|v| v as u8) else {
                    continue;
                };
                let d = parsed.get("d").unwrap_or(&Value::Null);

                match op {
                    rtc_op::HELLO => {
                        let interval_ms = d
                            .get("heartbeat_interval")
                            .and_then(Value::as_f64)
                            .map_or(15_000u64, |v| (v as u64).clamp(50, 120_000));
                        if let Some(prev) = heartbeat_task.take() {
                            prev.abort();
                        }
                        awaiting_heartbeat_ack
                            .store(false, std::sync::atomic::Ordering::Release);
                        let hb_tx = outbound_tx.clone();
                        let hb_seq = Arc::clone(&seq_ack);
                        let hb_awaiting = Arc::clone(&awaiting_heartbeat_ack);
                        let hb_control = control_tx.clone();
                        heartbeat_task = Some(tokio::spawn(async move {
                            let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
                            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            ticker.tick().await;
                            loop {
                                ticker.tick().await;
                                if hb_awaiting.swap(true, std::sync::atomic::Ordering::AcqRel) {
                                    warn!(
                                        "Stream RTC heartbeat ACK missed (zombie connection); closing socket"
                                    );
                                    let _ = hb_tx.send(WsMessage::Close(None)).await;
                                    let _ = hb_control
                                        .send("heartbeat ACK missed (zombie connection)".to_string())
                                        .await;
                                    break;
                                }
                                let nonce = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map_or(0, |d| d.as_millis() as u64);
                                let ack = hb_seq.load(std::sync::atomic::Ordering::Relaxed);
                                let hb = json!({
                                    "op": rtc_op::HEARTBEAT,
                                    "d": {
                                        "t": nonce,
                                        "seq_ack": ack,
                                    }
                                });
                                if let Ok(s) = serde_json::to_string(&hb)
                                    && hb_tx.send(WsMessage::text(s)).await.is_err()
                                {
                                    break;
                                }
                            }
                        }));
                    }
                    rtc_op::READY => {
                        let local = parse_ready_ssrcs(d);
                        *ssrcs_store.write().await = local.clone();
                        let _ = events_tx.send(StreamRtcEvent::Ready {
                            ssrcs: local.clone(),
                        });

                        let select = build_select_protocol_webrtc_payload(
                            &config.local_sdp_offer,
                            &config.codecs,
                        );
                        if let Ok(s) = serde_json::to_string(&select) {
                            let _ = outbound_tx.send(WsMessage::text(s)).await;
                        }

                        if config.broadcast_quality.is_some() {
                            let video_msg =
                                build_video_announcement_payload(&local, config.broadcast_quality);
                            if let Ok(s) = serde_json::to_string(&video_msg) {
                                let _ = outbound_tx.send(WsMessage::text(s)).await;
                            }
                        }
                    }
                    rtc_op::SESSION_DESCRIPTION => {
                        let remote_sdp = d
                            .get("sdp")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let video_codec = d
                            .get("video_codec")
                            .and_then(Value::as_str)
                            .and_then(VideoCodec::from_sdp_name)
                            .unwrap_or(VideoCodec::H264);
                        let media_session_id = d
                            .get("media_session_id")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                        let dave_ver = d
                            .get("dave_protocol_version")
                            .and_then(Value::as_u64)
                            .map_or(0, |v| v as u16);
                        negotiated_dave_version = dave_ver;
                        if dave_ver == 0 {
                            let _ = dave.set_passthrough_mode(true, Some(0));
                        }
                        let _ = events_tx.send(StreamRtcEvent::SessionDescription {
                            remote_sdp,
                            video_codec,
                            media_session_id,
                            dave_protocol_version: dave_ver,
                        });
                    }
                    rtc_op::CLIENTS_CONNECT => {
                        received_clients_connect = true;
                        if let Some(arr) = d.get("user_ids").and_then(Value::as_array) {
                            for uid in arr.iter().filter_map(Value::as_str) {
                                if let Ok(sf) = Snowflake::from_str(uid) {
                                    known_users.insert(sf.as_u64());
                                }
                            }
                        }
                    }
                    rtc_op::CLIENT_DISCONNECT => {
                        if let Some(uid) = d.get("user_id").and_then(Value::as_str)
                            && let Ok(sf) = Snowflake::from_str(uid)
                        {
                            known_users.remove(&sf.as_u64());
                        }
                    }
                    rtc_op::VIDEO => {
                        if let Some((uid, remote_ssrcs)) = parse_remote_video_ssrcs(d) {
                            known_users.insert(uid.as_u64());
                            let _ = events_tx.send(StreamRtcEvent::RemoteVideoSsrc {
                                user_id: uid,
                                ssrcs: remote_ssrcs.clone(),
                            });
                            if remote_ssrcs.video_ssrc != 0 {
                                let wants =
                                    build_media_sink_wants_payload(remote_ssrcs.video_ssrc, 100);
                                if let Ok(s) = serde_json::to_string(&wants) {
                                    let _ = outbound_tx.send(WsMessage::text(s)).await;
                                }
                            }
                        }
                    }
                    rtc_op::DAVE_PREPARE_TRANSITION => {
                        let transition_id = d
                            .get("transition_id")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u16;
                        let protocol_version = d
                            .get("protocol_version")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u16;
                        negotiated_dave_version = protocol_version;
                        if let Err(err) = dave.prepare_transition(transition_id, protocol_version) {
                            warn!(error = %err, "DAVE prepare_transition error");
                        } else if transition_id != 0 {
                            let ready = json!({
                                "op": rtc_op::DAVE_TRANSITION_READY,
                                "d": { "transition_id": transition_id }
                            });
                            if let Ok(s) = serde_json::to_string(&ready) {
                                let _ = outbound_tx.send(WsMessage::text(s)).await;
                            }
                        }
                        let _ = events_tx.send(StreamRtcEvent::DaveStateChanged(dave.session_state()));
                    }
                    rtc_op::DAVE_EXECUTE_TRANSITION => {
                        let transition_id = d
                            .get("transition_id")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u16;
                        if let Err(err) = dave.execute_transition(transition_id) {
                            warn!(error = %err, "DAVE execute_transition error");
                        }
                        let _ = events_tx.send(StreamRtcEvent::DaveStateChanged(dave.session_state()));
                    }
                    rtc_op::DAVE_PREPARE_EPOCH => {
                        let epoch = d.get("epoch").and_then(Value::as_u64).unwrap_or(0);
                        let protocol_version = d
                            .get("protocol_version")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u16;
                        negotiated_dave_version = protocol_version;
                        match dave.prepare_epoch(epoch, protocol_version) {
                            Ok(Some(kp)) => {
                                let bin = encode_client_dave_binary_frame(
                                    rtc_op::DAVE_MLS_KEY_PACKAGE,
                                    &kp,
                                );
                                let _ = outbound_tx.send(WsMessage::binary(bin)).await;
                            }
                            Ok(None) => {}
                            Err(err) => warn!(error = %err, "DAVE prepare_epoch failed"),
                        }
                        let _ = events_tx.send(StreamRtcEvent::DaveStateChanged(dave.session_state()));
                    }
                    rtc_op::HEARTBEAT_ACK => {
                        awaiting_heartbeat_ack
                            .store(false, std::sync::atomic::Ordering::Release);
                        trace!("received Stream RTC HEARTBEAT_ACK");
                    }
                    other => {
                        trace!(op = other, "unhandled Stream RTC JSON opcode");
                    }
                }
            }
            Some(Ok(WsMessage::Binary(bin))) => {
                let frame = match parse_server_dave_binary_frame(&bin) {
                    Ok(frame) => frame,
                    Err(err) => {
                        warn!(error = %err, "failed to parse server DAVE binary frame");
                        continue;
                    }
                };
                match frame {
                    ServerDaveFrame::ExternalSender { seq, payload } => {
                        seq_ack.store(i64::from(seq), std::sync::atomic::Ordering::Relaxed);
                        match dave.set_external_sender(&payload) {
                            Ok(kp) => {
                                let msg = encode_client_dave_binary_frame(
                                    rtc_op::DAVE_MLS_KEY_PACKAGE,
                                    &kp,
                                );
                                let _ = outbound_tx.send(WsMessage::binary(msg)).await;
                            }
                            Err(err) => warn!(error = %err, "DAVE set_external_sender failed"),
                        }
                    }
                    ServerDaveFrame::Proposals {
                        seq,
                        operation_type,
                        proposals,
                    } => {
                        seq_ack.store(i64::from(seq), std::sync::atomic::Ordering::Relaxed);
                        let expected_vec: Vec<u64> = known_users.iter().copied().collect();
                        let expected_opt = received_clients_connect.then_some(expected_vec.as_slice());
                        match dave.process_proposals(operation_type, &proposals, expected_opt) {
                            Ok(Some(cw)) => {
                                let msg = encode_dave_commit_welcome(&cw);
                                let _ = outbound_tx.send(WsMessage::binary(msg)).await;
                            }
                            Ok(None) => {}
                            Err(err) => warn!(error = %err, "DAVE process_proposals failed"),
                        }
                    }
                    ServerDaveFrame::AnnounceCommitTransition {
                        seq,
                        transition_id,
                        commit,
                    } => {
                        seq_ack.store(i64::from(seq), std::sync::atomic::Ordering::Relaxed);
                        match dave.process_commit(&commit) {
                            Ok(true) => {
                                let ver = negotiated_dave_version.max(DAVE_PROTOCOL_VERSION);
                                if dave.prepare_transition(transition_id, ver).is_ok()
                                    && transition_id != 0
                                {
                                    let ready = json!({
                                        "op": rtc_op::DAVE_TRANSITION_READY,
                                        "d": { "transition_id": transition_id }
                                    });
                                    if let Ok(s) = serde_json::to_string(&ready) {
                                        let _ = outbound_tx.send(WsMessage::text(s)).await;
                                    }
                                }
                            }
                            Ok(false) => {}
                            Err(err) => {
                                warn!(error = %err, transition_id, "invalid DAVE commit; sending Op 31 and re-uploading KeyPackage");
                                send_dave_invalid_and_recover(
                                    &dave,
                                    &outbound_tx,
                                    transition_id,
                                    negotiated_dave_version,
                                )
                                .await;
                            }
                        }
                        let _ = events_tx.send(StreamRtcEvent::DaveStateChanged(dave.session_state()));
                    }
                    ServerDaveFrame::Welcome {
                        seq,
                        transition_id,
                        welcome,
                    } => {
                        seq_ack.store(i64::from(seq), std::sync::atomic::Ordering::Relaxed);
                        let recognized_vec: Vec<u64> = known_users.iter().copied().collect();
                        let recognized_opt =
                            received_clients_connect.then_some(recognized_vec.as_slice());
                        match dave.process_welcome(&welcome, recognized_opt) {
                            Ok(()) => {
                                let ver = negotiated_dave_version.max(DAVE_PROTOCOL_VERSION);
                                if dave.prepare_transition(transition_id, ver).is_ok()
                                    && transition_id != 0
                                {
                                    let ready = json!({
                                        "op": rtc_op::DAVE_TRANSITION_READY,
                                        "d": { "transition_id": transition_id }
                                    });
                                    if let Ok(s) = serde_json::to_string(&ready) {
                                        let _ = outbound_tx.send(WsMessage::text(s)).await;
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, transition_id, "invalid DAVE welcome; sending Op 31 and re-uploading KeyPackage");
                                send_dave_invalid_and_recover(
                                    &dave,
                                    &outbound_tx,
                                    transition_id,
                                    negotiated_dave_version,
                                )
                                .await;
                            }
                        }
                        let _ = events_tx.send(StreamRtcEvent::DaveStateChanged(dave.session_state()));
                    }
                }
            }
            Some(Ok(WsMessage::Close(frame))) => {
                let reason = frame.map_or_else(
                    || "server closed Stream RTC connection".to_string(),
                    |f| format!("closed ({}): {}", f.code, f.reason),
                );
                break reason;
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => {
                break format!("Stream RTC transport error: {err}");
            }
            None => {
                break "Stream RTC stream ended".to_string();
            }
        }
    };

    if let Some(hb) = heartbeat_task.take() {
        hb.abort();
    }
    writer_task.abort();
    let _ = events_tx.send(StreamRtcEvent::Disconnected {
        reason: disconnect_reason,
    });
}

async fn send_dave_invalid_and_recover(
    dave: &DaveVideoContext,
    outbound_tx: &mpsc::Sender<WsMessage>,
    transition_id: u16,
    protocol_version: u16,
) {
    let invalid = json!({
        "op": rtc_op::DAVE_MLS_INVALID_COMMIT_WELCOME,
        "d": { "transition_id": transition_id }
    });
    if let Ok(s) = serde_json::to_string(&invalid) {
        let _ = outbound_tx.send(WsMessage::text(s)).await;
    }

    match dave.recover_from_invalid_commit_welcome(protocol_version) {
        Ok(kp) => {
            let msg = encode_client_dave_binary_frame(rtc_op::DAVE_MLS_KEY_PACKAGE, &kp);
            let _ = outbound_tx.send(WsMessage::binary(msg)).await;
        }
        Err(err) => {
            warn!(
                error = %err,
                transition_id,
                "failed to reinit DAVE session and regenerate KeyPackage after Op 31"
            );
        }
    }
}

fn parse_ready_ssrcs(d: &Value) -> StreamSsrcMap {
    let audio_ssrc = d.get("ssrc").and_then(Value::as_u64).unwrap_or(0) as u32;
    if let Some(stream) = d
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    {
        let video_ssrc = stream.get("ssrc").and_then(Value::as_u64).unwrap_or(0) as u32;
        let rtx_ssrc = stream
            .get("rtx_ssrc")
            .and_then(Value::as_u64)
            .map_or(video_ssrc.saturating_add(1), |v| v as u32);
        let rid = stream
            .get("rid")
            .and_then(Value::as_str)
            .unwrap_or("100")
            .to_string();
        let quality = stream
            .get("quality")
            .and_then(Value::as_u64)
            .unwrap_or(100) as u32;
        let active = stream
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        StreamSsrcMap {
            audio_ssrc,
            video_ssrc,
            rtx_ssrc,
            rid,
            quality,
            active,
        }
    } else {
        StreamSsrcMap {
            audio_ssrc,
            video_ssrc: audio_ssrc.saturating_add(1),
            rtx_ssrc: audio_ssrc.saturating_add(2),
            ..StreamSsrcMap::default()
        }
    }
}

fn parse_remote_video_ssrcs(d: &Value) -> Option<(Snowflake, StreamSsrcMap)> {
    let user_id = d
        .get("user_id")
        .and_then(Value::as_str)
        .and_then(|s| Snowflake::from_str(s).ok())?;
    let audio_ssrc = d.get("audio_ssrc").and_then(Value::as_u64).unwrap_or(0) as u32;
    let top_video_ssrc = d.get("video_ssrc").and_then(Value::as_u64).unwrap_or(0) as u32;

    if let Some(stream) = d
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    {
        let video_ssrc = stream
            .get("ssrc")
            .and_then(Value::as_u64)
            .map_or(top_video_ssrc, |v| v as u32);
        let rtx_ssrc = stream
            .get("rtx_ssrc")
            .and_then(Value::as_u64)
            .map_or(video_ssrc.saturating_add(1), |v| v as u32);
        let rid = stream
            .get("rid")
            .and_then(Value::as_str)
            .unwrap_or("100")
            .to_string();
        let quality = stream
            .get("quality")
            .and_then(Value::as_u64)
            .unwrap_or(100) as u32;
        let active = stream
            .get("active")
            .and_then(Value::as_bool)
            .unwrap_or(video_ssrc != 0);
        Some((
            user_id,
            StreamSsrcMap {
                audio_ssrc,
                video_ssrc,
                rtx_ssrc,
                rid,
                quality,
                active,
            },
        ))
    } else {
        let rtx_ssrc = d
            .get("rtx_ssrc")
            .and_then(Value::as_u64)
            .map_or(top_video_ssrc.saturating_add(1), |v| v as u32);
        Some((
            user_id,
            StreamSsrcMap {
                audio_ssrc,
                video_ssrc: top_video_ssrc,
                rtx_ssrc,
                rid: "100".to_string(),
                quality: 100,
                active: top_video_ssrc != 0,
            },
        ))
    }
}

fn parse_required_stream_key(data: &Value) -> CoreResult<StreamKey> {
    let raw = data
        .get("stream_key")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Protocol("Go Live dispatch missing stream_key".to_string()))?;
    StreamKey::parse(raw)
}

fn parse_required_snowflake(data: &Value, field: &str) -> CoreResult<Snowflake> {
    let value = data
        .get(field)
        .ok_or_else(|| CoreError::Protocol(format!("Go Live dispatch missing `{field}`")))?;
    if let Some(s) = value.as_str() {
        Snowflake::from_str(s)
    } else if let Some(n) = value.as_u64() {
        Ok(Snowflake::new(n))
    } else {
        Err(CoreError::Protocol(format!(
            "Go Live dispatch field `{field}` is not a Snowflake"
        )))
    }
}

fn parse_optional_snowflake(data: &Value, field: &str) -> CoreResult<Option<Snowflake>> {
    let Some(value) = data.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    if let Some(s) = value.as_str() {
        Ok(Some(Snowflake::from_str(s)?))
    } else if let Some(n) = value.as_u64() {
        Ok(Some(Snowflake::new(n)))
    } else {
        Err(CoreError::Protocol(format!(
            "Go Live dispatch field `{field}` has invalid Snowflake type"
        )))
    }
}

fn parse_snowflake_array(data: &Value, field: &str) -> CoreResult<Vec<Snowflake>> {
    let Some(arr) = data.get(field).and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        if let Some(s) = entry.as_str() {
            out.push(Snowflake::from_str(s)?);
        } else if let Some(n) = entry.as_u64() {
            out.push(Snowflake::new(n));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_key_guild_and_call_roundtrip() {
        let guild_key = StreamKey::parse("guild:100:200:300").expect("valid guild key");
        assert_eq!(guild_key.guild_id(), Some(Snowflake::new(100)));
        assert_eq!(guild_key.channel_id(), Snowflake::new(200));
        assert_eq!(guild_key.user_id(), Snowflake::new(300));
        assert!(!guild_key.is_call());
        assert_eq!(guild_key.to_canonical_string(), "guild:100:200:300");

        let call_key = StreamKey::parse("call:200:300").expect("valid DM/Group DM call key");
        assert_eq!(call_key.guild_id(), None);
        assert_eq!(call_key.channel_id(), Snowflake::new(200));
        assert_eq!(call_key.user_id(), Snowflake::new(300));
        assert!(call_key.is_call());
        assert_eq!(call_key.to_canonical_string(), "call:200:300");

        assert!(StreamKey::parse("guild:100:200").is_err());
        assert!(StreamKey::parse("unknown:1:2").is_err());
        assert!(StreamKey::parse("call:abc:123").is_err());
    }

    #[test]
    fn gateway_commands_and_dispatch_parsing() {
        let key = StreamKey::Call {
            channel_id: Snowflake::new(42),
            user_id: Snowflake::new(99),
        };
        let create = GatewayStreamCommand::Create {
            stream_key: key,
            preferred_region: Some("us-central".to_string()),
        }
        .to_gateway_payload();
        assert_eq!(create["op"], 18);
        assert_eq!(create["d"]["type"], "call");
        assert!(create["d"]["guild_id"].is_null());
        assert_eq!(create["d"]["channel_id"], "42");

        let watch = GatewayStreamCommand::Watch { stream_key: key }.to_gateway_payload();
        assert_eq!(watch["op"], 20);
        assert_eq!(watch["d"]["stream_key"], "call:42:99");

        let paused = GatewayStreamCommand::SetPaused {
            stream_key: key,
            paused: true,
        }
        .to_gateway_payload();
        assert_eq!(paused["op"], 22);
        assert_eq!(paused["d"]["paused"], true);

        let del = GatewayStreamCommand::Delete { stream_key: key }.to_gateway_payload();
        assert_eq!(del["op"], 19);

        let srv_update = json!({
            "stream_key": "call:42:99",
            "endpoint": "us-central.discord.media:443",
            "token": "synthetic-ephemeral-rtc-token"
        });
        let ev = parse_gateway_dispatch("STREAM_SERVER_UPDATE", &srv_update)
            .expect("parse dispatch")
            .expect("should produce event");
        let debug_repr = format!("{ev:?}");
        assert!(debug_repr.contains("[REDACTED]"));
        assert!(!debug_repr.contains("synthetic-ephemeral-rtc-token"));
    }

    #[test]
    fn dave_binary_framing_uses_big_endian_seq_and_transition_id() {
        // Op 29: seq=0x0102, op=29, transition_id=0x0304, commit=[0xAA, 0xBB]
        let raw = vec![0x01, 0x02, rtc_op::DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION, 0x03, 0x04, 0xAA, 0xBB];
        let parsed = parse_server_dave_binary_frame(&raw).expect("parse Op 29");
        assert_eq!(
            parsed,
            ServerDaveFrame::AnnounceCommitTransition {
                seq: 0x0102,
                transition_id: 0x0304,
                commit: vec![0xAA, 0xBB],
            }
        );

        // Op 27: seq=5, op=27, op_type=0 (APPEND), payload=[1, 2, 3]
        let prop_raw = vec![0x00, 0x05, rtc_op::DAVE_MLS_PROPOSALS, 0x00, 1, 2, 3];
        let parsed_prop = parse_server_dave_binary_frame(&prop_raw).expect("parse Op 27");
        assert_eq!(
            parsed_prop,
            ServerDaveFrame::Proposals {
                seq: 5,
                operation_type: ProposalsOperationType::APPEND,
                proposals: vec![1, 2, 3],
            }
        );

        let client_kp = encode_client_dave_binary_frame(rtc_op::DAVE_MLS_KEY_PACKAGE, &[9, 8, 7]);
        assert_eq!(client_kp, vec![rtc_op::DAVE_MLS_KEY_PACKAGE, 9, 8, 7]);
    }

    #[test]
    fn select_protocol_and_endpoint_normalization() {
        assert_eq!(
            normalize_rtc_endpoint_url("us-central1.discord.media:443"),
            "wss://us-central1.discord.media:443/?v=8"
        );
        assert_eq!(
            normalize_rtc_endpoint_url("wss://example.invalid/?v=8"),
            "wss://example.invalid/?v=8"
        );

        let specs = VideoCodec::negotiate_host_codec_specs();
        let payload = build_select_protocol_webrtc_payload("v=0\r\n", &specs);
        assert_eq!(payload["op"], 1);
        assert_eq!(payload["d"]["protocol"], "webrtc");
        assert!(payload["d"]["codecs"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[tokio::test]
    async fn zombie_heartbeat_disconnects_on_missed_ack() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local mock WS");
        let addr = listener.local_addr().expect("local addr");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("ws handshake");
            // Read IDENTIFY then send HELLO with 60ms heartbeat interval and never send HEARTBEAT_ACK.
            let _ = ws.next().await;
            let hello = json!({ "op": rtc_op::HELLO, "d": { "heartbeat_interval": 60 } });
            let _ = ws.send(WsMessage::text(hello.to_string())).await;
            // Drain incoming messages without acknowledging heartbeats until client disconnects.
            while let Some(Ok(msg)) = ws.next().await {
                if matches!(msg, WsMessage::Close(_)) {
                    break;
                }
            }
        });

        let dave = DaveVideoContext::new(Snowflake::new(10), Snowflake::new(20), 1).expect("dave");
        let cfg = StreamRtcConfig {
            stream_key: StreamKey::Call {
                channel_id: Snowflake::new(20),
                user_id: Snowflake::new(10),
            },
            rtc_server_id: Snowflake::new(99),
            user_id: Snowflake::new(10),
            session_id: "sess".to_string(),
            endpoint: format!("ws://{addr}"),
            token: RedactedStreamToken::new("tok".to_string()),
            codecs: Vec::new(),
            local_sdp_offer: "v=0\r\n".to_string(),
            broadcast_quality: None,
            enable_dave: true,
        };

        let mut client = StreamRtcClient::connect(cfg, dave).await.expect("connect");
        let mut rx = client.subscribe();

        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(ev @ StreamRtcEvent::Disconnected { .. }) = rx.recv().await {
                    return ev;
                }
            }
        })
        .await
        .expect("zombie connection should disconnect promptly on missed ACK");

        match event {
            StreamRtcEvent::Disconnected { reason } => {
                assert!(
                    reason.contains("heartbeat ACK missed"),
                    "unexpected disconnect reason: {reason}"
                );
            }
            other => panic!("expected Disconnected event, got {other:?}"),
        }

        client.disconnect().await;
        let _ = server.await;
    }

    #[tokio::test]
    async fn invalid_dave_welcome_sends_op31_and_reuploads_key_package() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local mock WS");
        let addr = listener.local_addr().expect("local addr");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("ws handshake");
            // Read IDENTIFY.
            let _ = ws.next().await;
            // Send corrupt Op 30 (Welcome): seq=1, op=30, transition_id=7, garbage welcome bytes.
            let corrupt_welcome = vec![0x00, 0x01, rtc_op::DAVE_MLS_WELCOME, 0x00, 0x07, 0xDE, 0xAD, 0xBE, 0xEF];
            ws.send(WsMessage::binary(corrupt_welcome))
                .await
                .expect("send corrupt welcome");

            // Expect JSON Op 31 DAVE_MLS_INVALID_COMMIT_WELCOME followed by binary Op 26 KeyPackage.
            let mut saw_op31 = false;
            let mut saw_op26 = false;
            while let Ok(Some(Ok(msg))) =
                tokio::time::timeout(Duration::from_secs(2), ws.next()).await
            {
                match msg {
                    WsMessage::Text(txt) => {
                        if let Ok(val) = serde_json::from_str::<Value>(txt.as_str())
                            && val["op"] == rtc_op::DAVE_MLS_INVALID_COMMIT_WELCOME
                            && val["d"]["transition_id"] == 7
                        {
                            saw_op31 = true;
                        }
                    }
                    WsMessage::Binary(bin)
                        if bin.first().copied() == Some(rtc_op::DAVE_MLS_KEY_PACKAGE)
                            && bin.len() > 16 =>
                    {
                        saw_op26 = true;
                    }
                    _ => {}
                }
                if saw_op31 && saw_op26 {
                    break;
                }
            }
            (saw_op31, saw_op26)
        });

        let dave = DaveVideoContext::new(Snowflake::new(10), Snowflake::new(20), 1).expect("dave");
        let cfg = StreamRtcConfig {
            stream_key: StreamKey::Call {
                channel_id: Snowflake::new(20),
                user_id: Snowflake::new(10),
            },
            rtc_server_id: Snowflake::new(99),
            user_id: Snowflake::new(10),
            session_id: "sess".to_string(),
            endpoint: format!("ws://{addr}"),
            token: RedactedStreamToken::new("tok".to_string()),
            codecs: Vec::new(),
            local_sdp_offer: "v=0\r\n".to_string(),
            broadcast_quality: None,
            enable_dave: true,
        };

        let mut client = StreamRtcClient::connect(cfg, dave).await.expect("connect");
        let (saw_op31, saw_op26) = server.await.expect("server task");
        assert!(saw_op31, "client must report Op 31 on invalid DAVE welcome");
        assert!(
            saw_op26,
            "client must re-init MLS and upload fresh Op 26 KeyPackage after Op 31"
        );
        client.disconnect().await;
    }
}
