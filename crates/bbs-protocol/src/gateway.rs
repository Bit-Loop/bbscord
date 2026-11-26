//! Discord Gateway v10 state machine (`wss://gateway.discord.gg/?v=10&encoding=json&compress=zlib-stream`).
//!
//! Features:
//! - Streaming `zlib-stream` inflater (`flate2::Decompress`) with continuous sliding window and
//!   zip-bomb memory guard.
//! - Jittered heartbeat loop with ACK-timeout zombie detection and automatic `RESUME` reconnect.
//! - User-account `IDENTIFY` (capabilities bitmask matching `discordo`) & `RESUME`.
//! - Full dispatch parsing (`READY`, `READY_SUPPLEMENTAL`, messages, reactions, typing, presences,
//!   channels & Group DM recipients, voice states/server updates, DM/Group DM calls, Go Live streams).
//! - Outbound signaling commands: Op `4` (`UpdateVoiceState` for guild & DM/Group DM calls),
//!   Op `14`/`37` (lazy guild subscriptions), and Op `18`/`19`/`20`/`22` (Go Live streams).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::time::{Duration, Instant};

use bbs_core::{
    BackendEvent, CallRingState, Channel, ChannelKind, ConnectionState, CoreError, CoreResult,
    GuildFolder, GuildMemberSummary, Message, MessagePatch, Reaction, Role, SecretToken, Snowflake,
    StreamDescriptor, User, VoiceParticipant,
};
use flate2::{Decompress, FlushDecompress, Status as FlateStatus};
use futures::{SinkExt, StreamExt};
use http::header;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};
use url::Url;
use zeroize::Zeroizing;

use crate::fingerprint::{DISCORD_ORIGIN, DiscordFingerprint, IdentifyProperties};
use crate::wire::{
    WireChannel, WireEmoji, WireGuild, WireGuildFolder, WireGuildMember, WireMessage,
    WireMessagePatch, WirePresence, WireRole, WireUser, WireVoiceState, deserialize_null_default,
};

/// Production Discord Gateway v10 WebSocket URL with streaming zlib compression.
pub const DISCORD_GATEWAY_URL: &str =
    "wss://gateway.discord.gg/?v=10&encoding=json&compress=zlib-stream";

/// Four-byte `Z_SYNC_FLUSH` trailer delimiting complete Discord `zlib-stream` frames.
pub const ZLIB_SYNC_FLUSH_SUFFIX: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF];

/// Safety cap on a single decompressed Gateway JSON message (32 MiB).
pub const MAX_GATEWAY_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;

/// Discord Gateway v10 opcodes.
pub mod opcodes {
    /// Server -> Client event dispatch.
    pub const DISPATCH: u8 = 0;
    /// Bidirectional heartbeat.
    pub const HEARTBEAT: u8 = 1;
    /// Client -> Server initial identify.
    pub const IDENTIFY: u8 = 2;
    /// Client -> Server presence update.
    pub const PRESENCE_UPDATE: u8 = 3;
    /// Client -> Server voice state update (guild voice or DM/Group DM call).
    pub const VOICE_STATE_UPDATE: u8 = 4;
    /// Client -> Server session resume.
    pub const RESUME: u8 = 6;
    /// Server -> Client reconnect request.
    pub const RECONNECT: u8 = 7;
    /// Client -> Server request guild members (`GUILD_MEMBERS_CHUNK` response).
    pub const REQUEST_GUILD_MEMBERS: u8 = 8;
    /// Server -> Client invalid session notice.
    pub const INVALID_SESSION: u8 = 9;
    /// Server -> Client hello with heartbeat interval.
    pub const HELLO: u8 = 10;
    /// Server -> Client heartbeat acknowledgement.
    pub const HEARTBEAT_ACK: u8 = 11;
    /// Client -> Server lazy guild channel subscription (single guild).
    pub const LAZY_REQUEST: u8 = 14;
    /// Client -> Server Go Live stream create.
    pub const STREAM_CREATE: u8 = 18;
    /// Client -> Server Go Live stream delete.
    pub const STREAM_DELETE: u8 = 19;
    /// Client -> Server Go Live stream watch.
    pub const STREAM_WATCH: u8 = 20;
    /// Client -> Server Go Live stream pause/unpause.
    pub const STREAM_SET_PAUSED: u8 = 22;
    /// Client -> Server bulk guild subscriptions (modern web client).
    pub const GUILD_SUBSCRIPTIONS_BULK: u8 = 37;
}

/// User-account Gateway `IDENTIFY` capability bitflags (matching `discordo/internal/ui/chat/model.go`).
pub mod capabilities {
    /// Fetch user notes lazily.
    pub const LAZY_USER_NOTES: u32 = 1 << 0;
    /// Omit affine user IDs.
    pub const NO_AFFINE_USER_IDS: u32 = 1 << 1;
    /// Versioned `read_state` payload in `READY`.
    pub const VERSIONED_READ_STATES: u32 = 1 << 2;
    /// Versioned `user_guild_settings` payload in `READY`.
    pub const VERSIONED_USER_GUILD_SETTINGS: u32 = 1 << 3;
    /// Deduplicate user objects into `READY.users` and use `recipient_ids` in `private_channels`.
    pub const DEDUPE_USER_OBJECTS: u32 = 1 << 4;
    /// Split initial state across `READY` and `READY_SUPPLEMENTAL`.
    pub const PRIORITIZED_READY_PAYLOAD: u32 = 1 << 5;
    /// Multiple guild experiment populations.
    pub const MULTIPLE_GUILD_EXPERIMENT_POPULATIONS: u32 = 1 << 6;
    /// Include non-channel read states.
    pub const NON_CHANNEL_READ_STATES: u32 = 1 << 7;
    /// Support auth token refresh notices.
    pub const AUTH_TOKEN_REFRESH: u32 = 1 << 8;
    /// Debounce bursts of message reactions via `MESSAGE_REACTION_ADD_MANY`.
    pub const DEBOUNCE_MESSAGE_REACTIONS: u32 = 1 << 13;

    /// Exact combined user capability bitmask used by `discordo` (`8701`).
    pub const DEFAULT_USER_CAPABILITIES: u32 = LAZY_USER_NOTES
        | VERSIONED_READ_STATES
        | VERSIONED_USER_GUILD_SETTINGS
        | DEDUPE_USER_OBJECTS
        | PRIORITIZED_READY_PAYLOAD
        | MULTIPLE_GUILD_EXPERIMENT_POPULATIONS
        | NON_CHANNEL_READ_STATES
        | AUTH_TOKEN_REFRESH
        | DEBOUNCE_MESSAGE_REACTIONS;
}

/// Stateful Discord `zlib-stream` decompressor maintaining one continuous zlib context per connection.
pub struct ZlibStreamInflater {
    decompress: Decompress,
    pending_compressed: Vec<u8>,
    max_output_bytes: usize,
}

impl fmt::Debug for ZlibStreamInflater {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZlibStreamInflater")
            .field("pending_compressed_len", &self.pending_compressed.len())
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

impl Default for ZlibStreamInflater {
    fn default() -> Self {
        Self::new(MAX_GATEWAY_PAYLOAD_BYTES)
    }
}

impl ZlibStreamInflater {
    /// Create a fresh streaming zlib inflater capped at `max_output_bytes` per message.
    #[must_use]
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            decompress: Decompress::new(true),
            pending_compressed: Vec::new(),
            max_output_bytes,
        }
    }

    /// Reset internal zlib dictionary and pending buffer (called on every new WebSocket connection).
    pub fn reset(&mut self) {
        self.decompress = Decompress::new(true);
        self.pending_compressed.clear();
    }

    /// Push a binary WebSocket frame chunk.
    ///
    /// Returns `Ok(Some(json_bytes))` once a frame terminates with the `0x00 0x00 0xFF 0xFF`
    /// sync-flush marker, `Ok(None)` when awaiting additional fragmented binary frames, or an
    /// error if decompression fails or exceeds [`Self::max_output_bytes`].
    pub fn push_chunk(&mut self, chunk: &[u8]) -> CoreResult<Option<Vec<u8>>> {
        if self.pending_compressed.len().saturating_add(chunk.len()) > self.max_output_bytes {
            self.pending_compressed.clear();
            return Err(CoreError::Protocol(
                "gateway compressed frame exceeded maximum buffer limit".to_owned(),
            ));
        }
        self.pending_compressed.extend_from_slice(chunk);

        if self.pending_compressed.len() < 4
            || self.pending_compressed[self.pending_compressed.len() - 4..]
                != ZLIB_SYNC_FLUSH_SUFFIX
        {
            return Ok(None);
        }

        let compressed = std::mem::take(&mut self.pending_compressed);
        let mut output = Vec::with_capacity(compressed.len().saturating_mul(2).min(64 * 1024));
        let mut scratch = [0u8; 8192];
        let mut offset = 0usize;

        loop {
            let input_slice = if offset < compressed.len() {
                &compressed[offset..]
            } else {
                &[]
            };
            let before_in = self.decompress.total_in();
            let before_out = self.decompress.total_out();

            let status = self
                .decompress
                .decompress(input_slice, &mut scratch, FlushDecompress::Sync)
                .map_err(|err| CoreError::Protocol(format!("zlib-stream inflate error: {err}")))?;

            let consumed = usize::try_from(self.decompress.total_in().saturating_sub(before_in))
                .unwrap_or(0);
            let produced = usize::try_from(self.decompress.total_out().saturating_sub(before_out))
                .unwrap_or(0);

            if produced > 0 {
                if output.len().saturating_add(produced) > self.max_output_bytes {
                    return Err(CoreError::Protocol(
                        "gateway decompressed payload exceeded maximum allowed size".to_owned(),
                    ));
                }
                output.extend_from_slice(&scratch[..produced]);
            }

            offset = offset.saturating_add(consumed);

            if status == FlateStatus::StreamEnd {
                break;
            }
            // Only exit once all compressed bytes are consumed AND zlib did not fill the scratch buffer
            // (a full scratch buffer may indicate more decompressed bytes remain buffered internally).
            if offset >= compressed.len() && produced < scratch.len() {
                break;
            }
            if consumed == 0 && produced == 0 {
                break;
            }
        }

        Ok(Some(output))
    }
}

/// Low-level signaling events emitted alongside [`BackendEvent`] for `bbs-voice` and `bbs-video`.
#[derive(Debug, Clone)]
pub enum GatewaySignalingEvent {
    /// Initial voice states captured from `READY` / `READY_SUPPLEMENTAL`.
    InitialVoiceStates {
        /// Guild ID (`None` for DM/Group DM calls).
        guild_id: Option<Snowflake>,
        /// Voice states active in that guild.
        states: Vec<WireVoiceState>,
    },
    /// Real-time `VOICE_STATE_UPDATE` dispatch.
    VoiceStateUpdate(WireVoiceState),
    /// Real-time `VOICE_SERVER_UPDATE` dispatch with zeroizing voice credentials.
    VoiceServerUpdate {
        /// Zeroizing voice session token for voice WebSocket `IDENTIFY`.
        token: SecretToken,
        /// Guild Snowflake (`None` for DM/Group DM calls).
        guild_id: Option<Snowflake>,
        /// Channel Snowflake (present for DM/Group DM calls).
        channel_id: Option<Snowflake>,
        /// Voice server hostname:port endpoint.
        endpoint: Option<String>,
    },
    /// Go Live `STREAM_CREATE` dispatch.
    StreamCreate {
        /// Canonical stream key (`"guild:<g>:<c>:<u>"` or `"call:<c>:<u>"`).
        stream_key: String,
        /// RTC server ID for voice/video signaling.
        rtc_server_id: String,
        /// Stream region.
        region: Option<String>,
        /// Active viewer user IDs.
        viewer_ids: Vec<Snowflake>,
        /// Whether the stream is currently paused.
        paused: bool,
    },
    /// Go Live `STREAM_SERVER_UPDATE` dispatch carrying stream RTC credentials.
    StreamServerUpdate {
        /// Zeroizing stream voice/WebRTC token.
        token: SecretToken,
        /// Canonical stream key.
        stream_key: String,
        /// Stream RTC endpoint hostname.
        endpoint: Option<String>,
    },
    /// Go Live `STREAM_DELETE` dispatch.
    StreamDelete {
        /// Canonical stream key that ended.
        stream_key: String,
        /// Optional server reason.
        reason: Option<String>,
    },
    /// Channel-scoped `CALL_CREATE` dispatch for a 1:1 DM or Group DM call.
    CallCreate {
        /// DM or Group DM channel Snowflake.
        channel_id: Snowflake,
        /// User IDs currently being rung.
        ringing: Vec<Snowflake>,
        /// Voice states already active in the call.
        voice_states: Vec<WireVoiceState>,
        /// Resolved caller user Snowflake when known.
        caller_id: Option<Snowflake>,
        /// Resolved caller display name when known.
        caller_name: Option<String>,
        /// Whether `channel_id` is a multi-user Group DM (`ChannelKind::GroupDm`).
        is_group_dm: bool,
    },
    /// Channel-scoped `CALL_UPDATE` dispatch for a 1:1 DM or Group DM call.
    CallUpdate {
        /// DM or Group DM channel Snowflake.
        channel_id: Snowflake,
        /// User IDs currently being rung.
        ringing: Vec<Snowflake>,
        /// Voice states currently active in the call.
        voice_states: Vec<WireVoiceState>,
        /// Resolved caller user Snowflake when known.
        caller_id: Option<Snowflake>,
        /// Resolved caller display name when known.
        caller_name: Option<String>,
        /// Whether `channel_id` is a multi-user Group DM (`ChannelKind::GroupDm`).
        is_group_dm: bool,
    },
    /// Channel-scoped `CALL_DELETE` dispatch indicating a DM/Group DM call ended.
    CallDelete {
        /// DM or Group DM channel Snowflake whose call ended.
        channel_id: Snowflake,
    },
}

/// Relationship summary captured from `READY.relationships`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipEntry {
    /// Related user Snowflake.
    pub user_id: Snowflake,
    /// Discord relationship type (`1` = friend, `2` = blocked, `3` = incoming, `4` = outgoing).
    pub kind: u8,
    /// Optional friend nickname.
    pub nickname: Option<String>,
}

/// Read-state entry captured from `READY.read_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadStateEntry {
    /// Channel Snowflake.
    pub channel_id: Snowflake,
    /// Last acknowledged message Snowflake.
    pub last_message_id: Option<Snowflake>,
    /// Unread direct mention count.
    pub mention_count: u32,
}

/// Guild channel subscription payload sent in Op `14` (`LAZY_REQUEST`) and Op `37` (`GUILD_SUBSCRIPTIONS_BULK`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuildSubscriptionPayload {
    /// Subscribe to typing events in this guild.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typing: Option<bool>,
    /// Subscribe to thread list/member updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<bool>,
    /// Subscribe to activity updates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activities: Option<bool>,
    /// Map of channel ID -> inclusive member list slice ranges (e.g. `[[0, 99]]`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub channels: HashMap<Snowflake, Vec<[u32; 2]>>,
    /// Specific member user IDs to request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Snowflake>,
}

/// Outbound commands sent from application/voice/video layers into the Gateway task.
#[derive(Debug, Clone)]
pub enum GatewayCommand {
    /// Op `4`: Join, move, update mute/deaf/video, or leave a guild voice channel or DM/Group DM call.
    UpdateVoiceState {
        /// Target guild Snowflake (`None` for 1:1 DM or Group DM calls).
        guild_id: Option<Snowflake>,
        /// Target voice/DM channel Snowflake (`None` to disconnect).
        channel_id: Option<Snowflake>,
        /// Self-mute state.
        self_mute: bool,
        /// Self-deafen state.
        self_deaf: bool,
        /// Camera video enabled.
        self_video: bool,
    },
    /// Op `8`: Request guild member metadata (`GUILD_MEMBERS_CHUNK`) by query prefix or user IDs.
    RequestGuildMembers {
        /// Target guild Snowflake.
        guild_id: Snowflake,
        /// Optional username/nickname prefix query (used when `user_ids` is empty).
        query: Option<String>,
        /// Optional explicit list of user Snowflakes to fetch.
        user_ids: Vec<Snowflake>,
        /// Maximum members to return when querying by prefix (`0` = all / server cap).
        limit: u32,
    },
    /// Op `14`: Single-guild lazy channel / member-list subscription.
    LazyRequest {
        /// Target guild Snowflake.
        guild_id: Snowflake,
        /// Subscription options and channel ranges.
        payload: GuildSubscriptionPayload,
    },
    /// Op `37`: Bulk guild subscriptions across multiple guilds.
    GuildSubscriptionsBulk {
        /// Map of `guild_id -> GuildSubscriptionPayload`.
        subscriptions: HashMap<Snowflake, GuildSubscriptionPayload>,
    },
    /// Op `18`: Create a Go Live screenshare stream in a guild voice channel or DM/Group DM call.
    StreamCreate {
        /// Guild Snowflake (`None` for `"call"` stream type, `Some` for `"guild"`).
        guild_id: Option<Snowflake>,
        /// Voice or call channel Snowflake.
        channel_id: Snowflake,
        /// Optional preferred RTC region.
        preferred_region: Option<String>,
    },
    /// Op `19`: End an active Go Live screenshare stream.
    StreamDelete {
        /// Canonical stream key (`"guild:<g>:<c>:<u>"` or `"call:<c>:<u>"`).
        stream_key: String,
    },
    /// Op `20`: Request stream credentials to watch a Go Live screenshare.
    StreamWatch {
        /// Canonical stream key.
        stream_key: String,
    },
    /// Op `22`: Pause or resume an outbound Go Live stream.
    StreamSetPaused {
        /// Canonical stream key.
        stream_key: String,
        /// `true` to pause video transmission, `false` to resume.
        paused: bool,
    },
    /// Gracefully close the Gateway connection and terminate the run loop.
    Shutdown,
}

/// Configuration knobs for [`DiscordGateway`].
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Initial Gateway WebSocket URL.
    pub gateway_url: String,
    /// User-account `IDENTIFY` capabilities bitmask.
    pub capabilities: u32,
    /// Initial reconnect backoff delay.
    pub initial_reconnect_backoff: Duration,
    /// Maximum reconnect backoff delay.
    pub max_reconnect_backoff: Duration,
    /// Optional fixed first-heartbeat jitter fraction in `(0.0, 1.0]` for deterministic tests.
    pub heartbeat_jitter_override: Option<f64>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            gateway_url: DISCORD_GATEWAY_URL.to_owned(),
            capabilities: capabilities::DEFAULT_USER_CAPABILITIES,
            initial_reconnect_backoff: Duration::from_secs(1),
            max_reconnect_backoff: Duration::from_secs(30),
            heartbeat_jitter_override: None,
        }
    }
}

/// Cloneable control handle for sending Gateway opcodes and subscribing to live events.
#[derive(Debug, Clone)]
pub struct GatewayHandle {
    cmd_tx: mpsc::UnboundedSender<GatewayCommand>,
    events_tx: broadcast::Sender<BackendEvent>,
    signaling_tx: broadcast::Sender<GatewaySignalingEvent>,
}

impl GatewayHandle {
    /// Send a raw [`GatewayCommand`] to the running Gateway task.
    pub fn send(&self, cmd: GatewayCommand) -> CoreResult<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| CoreError::Network("gateway command channel closed".to_owned()))
    }

    /// Send Op `4` (`UpdateVoiceState`) for a guild voice channel (`guild_id = Some(...)`)
    /// or a 1:1 / Group DM call (`guild_id = None`). Pass `channel_id = None` to disconnect.
    pub fn update_voice_state(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Option<Snowflake>,
        self_mute: bool,
        self_deaf: bool,
        self_video: bool,
    ) -> CoreResult<()> {
        self.send(GatewayCommand::UpdateVoiceState {
            guild_id,
            channel_id,
            self_mute,
            self_deaf,
            self_video,
        })
    }

    /// Send Op `8` (`REQUEST_GUILD_MEMBERS`) to resolve guild member nicknames and roles.
    pub fn request_guild_members(
        &self,
        guild_id: Snowflake,
        query: Option<String>,
        user_ids: Vec<Snowflake>,
    ) -> CoreResult<()> {
        self.send(GatewayCommand::RequestGuildMembers {
            guild_id,
            query,
            user_ids,
            limit: 25,
        })
    }

    /// Send Op `14` (`LAZY_REQUEST`) to subscribe to a guild channel's member list and typing.
    pub fn subscribe_guild_channel(
        &self,
        guild_id: Snowflake,
        channel_id: Snowflake,
        ranges: Vec<[u32; 2]>,
    ) -> CoreResult<()> {
        let mut channels = HashMap::new();
        channels.insert(channel_id, ranges);
        self.send(GatewayCommand::LazyRequest {
            guild_id,
            payload: GuildSubscriptionPayload {
                typing: Some(true),
                threads: Some(true),
                activities: Some(true),
                channels,
                members: Vec::new(),
            },
        })
    }

    /// Send Op `37` (`GUILD_SUBSCRIPTIONS_BULK`).
    pub fn subscribe_guilds_bulk(
        &self,
        subscriptions: HashMap<Snowflake, GuildSubscriptionPayload>,
    ) -> CoreResult<()> {
        self.send(GatewayCommand::GuildSubscriptionsBulk { subscriptions })
    }

    /// Send Op `18` (`STREAM_CREATE`).
    pub fn stream_create(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        preferred_region: Option<String>,
    ) -> CoreResult<()> {
        self.send(GatewayCommand::StreamCreate {
            guild_id,
            channel_id,
            preferred_region,
        })
    }

    /// Send Op `19` (`STREAM_DELETE`).
    pub fn stream_delete(&self, stream_key: impl Into<String>) -> CoreResult<()> {
        self.send(GatewayCommand::StreamDelete {
            stream_key: stream_key.into(),
        })
    }

    /// Send Op `20` (`STREAM_WATCH`).
    pub fn stream_watch(&self, stream_key: impl Into<String>) -> CoreResult<()> {
        self.send(GatewayCommand::StreamWatch {
            stream_key: stream_key.into(),
        })
    }

    /// Send Op `22` (`STREAM_SET_PAUSED`).
    pub fn stream_set_paused(
        &self,
        stream_key: impl Into<String>,
        paused: bool,
    ) -> CoreResult<()> {
        self.send(GatewayCommand::StreamSetPaused {
            stream_key: stream_key.into(),
            paused,
        })
    }

    /// Gracefully shut down the Gateway loop.
    pub fn shutdown(&self) -> CoreResult<()> {
        self.send(GatewayCommand::Shutdown)
    }

    /// Subscribe to core [`BackendEvent`] dispatches.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<BackendEvent> {
        self.events_tx.subscribe()
    }

    /// Subscribe to low-level voice/video [`GatewaySignalingEvent`] dispatches.
    #[must_use]
    pub fn subscribe_signaling(&self) -> broadcast::Receiver<GatewaySignalingEvent> {
        self.signaling_tx.subscribe()
    }
}

/// Ensure a Gateway base URL carries `?v=10&encoding=json&compress=zlib-stream` when missing.
#[must_use]
pub fn normalize_gateway_url(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return raw.to_owned();
    };
    let has_v = url.query_pairs().any(|(k, _)| k == "v");
    let has_enc = url.query_pairs().any(|(k, _)| k == "encoding");
    let has_comp = url.query_pairs().any(|(k, _)| k == "compress");
    {
        let mut qp = url.query_pairs_mut();
        if !has_v {
            qp.append_pair("v", "10");
        }
        if !has_enc {
            qp.append_pair("encoding", "json");
        }
        if !has_comp {
            qp.append_pair("compress", "zlib-stream");
        }
    }
    url.to_string()
}

/// Build an outbound JSON payload for a [`GatewayCommand`] (pure helper, unit-testable offline).
pub fn serialize_gateway_command(cmd: &GatewayCommand) -> CoreResult<Option<String>> {
    let value = match cmd {
        GatewayCommand::UpdateVoiceState {
            guild_id,
            channel_id,
            self_mute,
            self_deaf,
            self_video,
        } => serde_json::json!({
            "op": opcodes::VOICE_STATE_UPDATE,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": self_mute,
                "self_deaf": self_deaf,
                "self_video": self_video,
                "flags": 2
            }
        }),
        GatewayCommand::RequestGuildMembers {
            guild_id,
            query,
            user_ids,
            limit,
        } => {
            let d = if !user_ids.is_empty() {
                serde_json::json!({
                    "guild_id": guild_id,
                    "user_ids": user_ids,
                    "presences": false
                })
            } else {
                serde_json::json!({
                    "guild_id": guild_id,
                    "query": query.as_deref().unwrap_or(""),
                    "limit": limit,
                    "presences": false
                })
            };
            serde_json::json!({
                "op": opcodes::REQUEST_GUILD_MEMBERS,
                "d": d
            })
        }
        GatewayCommand::LazyRequest { guild_id, payload } => {
            let mut d = serde_json::to_value(payload)
                .map_err(|err| CoreError::Protocol(format!("serialize Op 14 payload: {err}")))?;
            if let Some(obj) = d.as_object_mut() {
                obj.insert(
                    "guild_id".to_owned(),
                    serde_json::Value::String(guild_id.to_string()),
                );
            }
            serde_json::json!({
                "op": opcodes::LAZY_REQUEST,
                "d": d
            })
        }
        GatewayCommand::GuildSubscriptionsBulk { subscriptions } => serde_json::json!({
            "op": opcodes::GUILD_SUBSCRIPTIONS_BULK,
            "d": {
                "subscriptions": subscriptions
            }
        }),
        GatewayCommand::StreamCreate {
            guild_id,
            channel_id,
            preferred_region,
        } => {
            let stream_type = if guild_id.is_some() { "guild" } else { "call" };
            serde_json::json!({
                "op": opcodes::STREAM_CREATE,
                "d": {
                    "type": stream_type,
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "preferred_region": preferred_region
                }
            })
        }
        GatewayCommand::StreamDelete { stream_key } => serde_json::json!({
            "op": opcodes::STREAM_DELETE,
            "d": { "stream_key": stream_key }
        }),
        GatewayCommand::StreamWatch { stream_key } => serde_json::json!({
            "op": opcodes::STREAM_WATCH,
            "d": { "stream_key": stream_key }
        }),
        GatewayCommand::StreamSetPaused { stream_key, paused } => serde_json::json!({
            "op": opcodes::STREAM_SET_PAUSED,
            "d": {
                "stream_key": stream_key,
                "paused": paused
            }
        }),
        GatewayCommand::Shutdown => return Ok(None),
    };

    serde_json::to_string(&value)
        .map(Some)
        .map_err(|err| CoreError::Protocol(format!("serialize gateway command: {err}")))
}

/// Build an outbound Op `2` (`IDENTIFY`) JSON string inside a [`Zeroizing<String>`].
pub fn build_identify_payload(
    token: &SecretToken,
    capabilities: u32,
    properties: &IdentifyProperties,
) -> CoreResult<Zeroizing<String>> {
    #[derive(Serialize)]
    struct IdentifyEnvelope<'a> {
        op: u8,
        d: IdentifyData<'a>,
    }
    #[derive(Serialize)]
    struct IdentifyData<'a> {
        token: &'a str,
        capabilities: u32,
        properties: &'a IdentifyProperties,
        presence: IdentifyPresence<'a>,
        compress: bool,
        client_state: IdentifyClientState,
    }
    #[derive(Serialize)]
    struct IdentifyPresence<'a> {
        status: &'a str,
        since: u64,
        activities: &'a [serde_json::Value],
        afk: bool,
    }
    #[derive(Serialize)]
    struct IdentifyClientState {
        guild_versions: HashMap<String, u64>,
    }

    let envelope = IdentifyEnvelope {
        op: opcodes::IDENTIFY,
        d: IdentifyData {
            token: token.expose_secret(),
            capabilities,
            properties,
            presence: IdentifyPresence {
                status: "unknown",
                since: 0,
                activities: &[],
                afk: false,
            },
            compress: false,
            client_state: IdentifyClientState {
                guild_versions: HashMap::new(),
            },
        },
    };

    let serialized = serde_json::to_string(&envelope)
        .map_err(|err| CoreError::Protocol(format!("serialize IDENTIFY: {err}")))?;
    Ok(Zeroizing::new(serialized))
}

/// Build an outbound Op `6` (`RESUME`) JSON string inside a [`Zeroizing<String>`].
pub fn build_resume_payload(
    token: &SecretToken,
    session_id: &str,
    seq: u64,
) -> CoreResult<Zeroizing<String>> {
    #[derive(Serialize)]
    struct ResumeEnvelope<'a> {
        op: u8,
        d: ResumeData<'a>,
    }
    #[derive(Serialize)]
    struct ResumeData<'a> {
        token: &'a str,
        session_id: &'a str,
        seq: u64,
    }

    let serialized = serde_json::to_string(&ResumeEnvelope {
        op: opcodes::RESUME,
        d: ResumeData {
            token: token.expose_secret(),
            session_id,
            seq,
        },
    })
    .map_err(|err| CoreError::Protocol(format!("serialize RESUME: {err}")))?;
    Ok(Zeroizing::new(serialized))
}

/// Build an outbound Op `1` (`HEARTBEAT`) JSON string.
pub fn build_heartbeat_payload(seq: Option<u64>) -> CoreResult<String> {
    serde_json::to_string(&serde_json::json!({
        "op": opcodes::HEARTBEAT,
        "d": seq
    }))
    .map_err(|err| CoreError::Protocol(format!("serialize HEARTBEAT: {err}")))
}

/// Stateful in-memory session cache maintained by the Gateway dispatcher.
#[derive(Debug, Default)]
pub struct GatewaySessionState {
    /// Current signed-in user once `READY` has been processed.
    pub current_user: Option<User>,
    /// Active Gateway session ID for `RESUME`.
    pub session_id: Option<String>,
    /// Server-provided Gateway resume URL.
    pub resume_gateway_url: Option<String>,
    /// Latest dispatch sequence number (`s`).
    pub sequence: Option<u64>,
    /// Cached users indexed by Snowflake (seeded from `READY.users`).
    pub users_by_id: HashMap<Snowflake, User>,
    /// Cached channels (guild channels, 1:1 DMs, Group DMs) indexed by Snowflake.
    pub channels_by_id: HashMap<Snowflake, Channel>,
    /// Cached read states indexed by channel Snowflake.
    pub read_states: HashMap<Snowflake, ReadStateEntry>,
    /// Cached relationships indexed by user Snowflake.
    pub relationships: HashMap<Snowflake, RelationshipEntry>,
    /// Cached voice states keyed by `(channel_id, user_id)`.
    pub voice_states: HashMap<Snowflake, HashMap<Snowflake, WireVoiceState>>,
    /// Active Go Live streams keyed by `stream_key`.
    pub active_streams: HashMap<String, StreamDescriptor>,
    /// Cached guild roles keyed by guild Snowflake.
    pub guild_roles: HashMap<Snowflake, Vec<Role>>,
    /// Per-channel mute overrides tracked from `READY.user_guild_settings` and `USER_GUILD_SETTINGS_UPDATE`.
    pub muted_channels: HashMap<Snowflake, bool>,
    /// Active/ringing DM & Group DM calls tracked by `channel_id` so `CALL_DELETE` is channel-scoped.
    pub active_calls: HashMap<Snowflake, CallRingState>,
    /// Currently surfaced DM/Group DM call channel ID (for channel-scoped `CALL_DELETE` clearing).
    pub active_call_channel: Option<Snowflake>,
}

impl GatewaySessionState {
    /// Process a single raw Gateway JSON payload, returning emitted core events, signaling events,
    /// and any control action required by the connection loop.
    pub fn handle_raw_frame(
        &mut self,
        payload_bytes: &[u8],
    ) -> CoreResult<GatewayFrameOutcome> {
        let frame: WireGatewayPayload = serde_json::from_slice(payload_bytes)
            .map_err(|err| CoreError::Protocol(format!("parse gateway frame JSON: {err}")))?;

        if let Some(seq) = frame.s {
            self.sequence = Some(seq);
        }

        let mut outcome = GatewayFrameOutcome::default();

        match frame.op {
            opcodes::HELLO => {
                let hello: WireHello = frame
                    .d
                    .ok_or_else(|| CoreError::Protocol("gateway HELLO missing data".to_owned()))
                    .and_then(|v| {
                        serde_json::from_value(v)
                            .map_err(|err| CoreError::Protocol(format!("parse HELLO: {err}")))
                    })?;
                outcome.control = Some(GatewayControl::Hello {
                    heartbeat_interval_ms: hello.heartbeat_interval.max(250),
                });
            }
            opcodes::HEARTBEAT => {
                outcome.control = Some(GatewayControl::SendHeartbeatNow);
            }
            opcodes::HEARTBEAT_ACK => {
                outcome.control = Some(GatewayControl::HeartbeatAck);
            }
            opcodes::RECONNECT => {
                outcome.control = Some(GatewayControl::Reconnect { resumable: true });
            }
            opcodes::INVALID_SESSION => {
                let resumable = frame
                    .d
                    .as_ref()
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !resumable {
                    self.session_id = None;
                    self.resume_gateway_url = None;
                    self.sequence = None;
                }
                outcome.control = Some(GatewayControl::InvalidSession { resumable });
            }
            opcodes::DISPATCH => {
                if let (Some(event_name), Some(data)) = (frame.t.as_deref(), frame.d) {
                    self.handle_dispatch(event_name, data, &mut outcome);
                }
            }
            other => {
                debug!(op = other, "ignoring unhandled gateway opcode");
            }
        }

        Ok(outcome)
    }

    fn handle_dispatch(
        &mut self,
        event_name: &str,
        data: serde_json::Value,
        outcome: &mut GatewayFrameOutcome,
    ) {
        match event_name {
            "READY" => self.handle_ready(data, outcome),
            "READY_SUPPLEMENTAL" => self.handle_ready_supplemental(data, outcome),
            "RESUMED" => {
                outcome.events.push(BackendEvent::ConnectionChanged {
                    state: ConnectionState::Ready,
                    latency_ms: None,
                    detail: Some("session resumed".to_owned()),
                });
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireChannel>(data) {
                    let ch = wire.into_core_channel(None, &self.users_by_id);
                    self.channels_by_id.insert(ch.id, ch.clone());
                    outcome.events.push(BackendEvent::ChannelUpserted(ch));
                }
            }
            "CHANNEL_DELETE" => {
                if let Ok(wire) = serde_json::from_value::<WireChannelDelete>(data) {
                    self.channels_by_id.remove(&wire.id);
                    outcome.events.push(BackendEvent::ChannelDeleted {
                        guild_id: wire.guild_id,
                        channel_id: wire.id,
                    });
                }
            }
            "CHANNEL_RECIPIENT_ADD" => {
                if let Ok(wire) = serde_json::from_value::<WireChannelRecipientEvent>(data) {
                    let user = User::from(wire.user);
                    self.users_by_id.insert(user.id, user.clone());
                    if let Some(ch) = self.channels_by_id.get_mut(&wire.channel_id) {
                        if !ch.recipients.iter().any(|r| r.id == user.id) {
                            ch.recipients.push(user);
                        }
                        outcome.events.push(BackendEvent::ChannelUpserted(ch.clone()));
                    }
                }
            }
            "CHANNEL_RECIPIENT_REMOVE" => {
                if let Ok(wire) = serde_json::from_value::<WireChannelRecipientEvent>(data)
                    && let Some(ch) = self.channels_by_id.get_mut(&wire.channel_id)
                {
                    ch.recipients.retain(|r| r.id != wire.user.id);
                    outcome.events.push(BackendEvent::ChannelUpserted(ch.clone()));
                }
            }
            "MESSAGE_CREATE" => {
                if let Ok(wire) = serde_json::from_value::<WireMessage>(data) {
                    let msg = Message::from(wire);
                    self.users_by_id.insert(msg.author.id, msg.author.clone());
                    outcome.events.push(BackendEvent::MessageCreated(msg));
                }
            }
            "MESSAGE_UPDATE" => {
                // Discord sends partial `MESSAGE_UPDATE` dispatches (e.g., link/GIFV embed unfurls
                // or pin toggles) omitting `author` and/or `content`. Route those to
                // `MessagePatched` so cached author/content/attachments are never clobbered.
                let is_full_message = data.get("author").is_some_and(|v| !v.is_null())
                    && data.get("content").is_some_and(|v| !v.is_null());
                if is_full_message {
                    if let Ok(wire) = serde_json::from_value::<WireMessage>(data) {
                        let msg = Message::from(wire);
                        self.users_by_id.insert(msg.author.id, msg.author.clone());
                        outcome.events.push(BackendEvent::MessageUpdated(msg));
                    }
                } else if let Ok(wire) = serde_json::from_value::<WireMessagePatch>(data) {
                    outcome
                        .events
                        .push(BackendEvent::MessagePatched(MessagePatch::from(wire)));
                }
            }
            "MESSAGE_ACK" => {
                if let Ok(wire) = serde_json::from_value::<WireMessageAck>(data)
                    && let Some(last_message_id) = wire.message_id
                {
                    let entry = ReadStateEntry {
                        channel_id: wire.channel_id,
                        last_message_id: Some(last_message_id),
                        mention_count: wire.mention_count,
                    };
                    self.read_states.insert(wire.channel_id, entry);
                    if let Some(ch) = self.channels_by_id.get_mut(&wire.channel_id) {
                        ch.last_ack_message_id = Some(last_message_id);
                        ch.mention_count = wire.mention_count;
                        if ch
                            .last_message_id
                            .is_none_or(|last| last <= last_message_id)
                        {
                            ch.unread_count = 0;
                        }
                    }
                    outcome.events.push(BackendEvent::MessageAcked {
                        channel_id: wire.channel_id,
                        last_message_id,
                        mention_count: wire.mention_count,
                    });
                }
            }
            "USER_GUILD_SETTINGS_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireUserGuildSettingsEntry>(data) {
                    self.apply_guild_settings_entry(wire, outcome);
                }
            }
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireGuildRoleUpsert>(data) {
                    let role = Role::from(wire.role);
                    let roles = self.guild_roles.entry(wire.guild_id).or_default();
                    if let Some(existing) = roles.iter_mut().find(|r| r.id == role.id) {
                        *existing = role;
                    } else {
                        roles.push(role);
                    }
                    roles.sort_by_key(|r| (r.position, r.id));
                    outcome.events.push(BackendEvent::GuildRolesUpdated {
                        guild_id: wire.guild_id,
                        roles: roles.clone(),
                    });
                }
            }
            "GUILD_ROLE_DELETE" => {
                if let Ok(wire) = serde_json::from_value::<WireGuildRoleDelete>(data) {
                    let roles = self.guild_roles.entry(wire.guild_id).or_default();
                    roles.retain(|r| r.id != wire.role_id);
                    outcome.events.push(BackendEvent::GuildRolesUpdated {
                        guild_id: wire.guild_id,
                        roles: roles.clone(),
                    });
                }
            }
            "GUILD_MEMBER_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireGuildMemberUpdate>(data) {
                    let user = User::from(wire.user);
                    let user_id = user.id;
                    self.users_by_id.insert(user_id, user);
                    outcome.events.push(BackendEvent::GuildMembersUpdated {
                        guild_id: wire.guild_id,
                        members: vec![GuildMemberSummary {
                            user_id,
                            nick: wire.nick.filter(|s| !s.is_empty()),
                            role_ids: wire.roles,
                        }],
                    });
                }
            }
            "GUILD_MEMBERS_CHUNK" => {
                if let Ok(wire) = serde_json::from_value::<WireGuildMembersChunk>(data) {
                    let mut members = Vec::with_capacity(wire.members.len());
                    for m in wire.members {
                        if let Some(ref wu) = m.user {
                            let u = User::from(wu.clone());
                            self.users_by_id.insert(u.id, u);
                        }
                        if let Some(summary) = m.to_member_summary(None) {
                            members.push(summary);
                        }
                    }
                    if !members.is_empty() {
                        outcome.events.push(BackendEvent::GuildMembersUpdated {
                            guild_id: wire.guild_id,
                            members,
                        });
                    }
                }
            }
            "GUILD_MEMBER_LIST_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireGuildMemberListUpdate>(data) {
                    let mut members = Vec::new();
                    for op in wire.ops {
                        let all_items = op.items.into_iter().chain(op.item);
                        for item in all_items {
                            if let Some(entry) = item.member {
                                if let Some(ref wu) = entry.member.user {
                                    let u = User::from(wu.clone());
                                    self.users_by_id.insert(u.id, u);
                                }
                                if let Some(summary) = entry.member.to_member_summary(None) {
                                    members.push(summary);
                                }
                                if let Some(wp) = entry.presence
                                    && let Some(presence) = wp.into_core()
                                {
                                    if let Some(u) = self.users_by_id.get_mut(&presence.user_id) {
                                        u.presence = presence.status;
                                    }
                                    outcome.events.push(BackendEvent::PresenceUpdated(presence));
                                }
                            }
                        }
                    }
                    if !members.is_empty() {
                        outcome.events.push(BackendEvent::GuildMembersUpdated {
                            guild_id: wire.guild_id,
                            members,
                        });
                    }
                }
            }
            "MESSAGE_DELETE" => {
                if let Ok(wire) = serde_json::from_value::<WireMessageDelete>(data) {
                    outcome.events.push(BackendEvent::MessageDeleted {
                        channel_id: wire.channel_id,
                        message_id: wire.id,
                    });
                }
            }
            "MESSAGE_REACTION_ADD" => {
                if let Ok(wire) = serde_json::from_value::<WireReactionAdd>(data) {
                    let by_me = self
                        .current_user
                        .as_ref()
                        .is_some_and(|u| u.id == wire.user_id);
                    outcome.events.push(BackendEvent::ReactionAdded {
                        channel_id: wire.channel_id,
                        message_id: wire.message_id,
                        reaction: Reaction {
                            emoji_id: wire.emoji.id,
                            emoji_name: wire.emoji.name.unwrap_or_default(),
                            animated: wire.emoji.animated,
                            count: 1,
                            me: by_me,
                        },
                    });
                }
            }
            "MESSAGE_REACTION_ADD_MANY" => {
                if let Ok(wire) = serde_json::from_value::<WireReactionAddMany>(data) {
                    let my_id = self.current_user.as_ref().map(|u| u.id);
                    for entry in wire.reactions {
                        let by_me = my_id.is_some_and(|me| entry.users.contains(&me));
                        let count = u32::try_from(entry.users.len()).unwrap_or(1).max(1);
                        outcome.events.push(BackendEvent::ReactionAdded {
                            channel_id: wire.channel_id,
                            message_id: wire.message_id,
                            reaction: Reaction {
                                emoji_id: entry.emoji.id,
                                emoji_name: entry.emoji.name.unwrap_or_default(),
                                animated: entry.emoji.animated,
                                count,
                                me: by_me,
                            },
                        });
                    }
                }
            }
            "MESSAGE_REACTION_REMOVE" => {
                if let Ok(wire) = serde_json::from_value::<WireReactionRemove>(data) {
                    let by_me = self
                        .current_user
                        .as_ref()
                        .is_some_and(|u| u.id == wire.user_id);
                    outcome.events.push(BackendEvent::ReactionRemoved {
                        channel_id: wire.channel_id,
                        message_id: wire.message_id,
                        emoji_id: wire.emoji.id,
                        emoji_name: wire.emoji.name.unwrap_or_default(),
                        by_me,
                    });
                }
            }
            "MESSAGE_REACTION_REMOVE_EMOJI" => {
                if let Ok(wire) = serde_json::from_value::<WireReactionRemoveEmoji>(data) {
                    outcome.events.push(BackendEvent::ReactionRemoved {
                        channel_id: wire.channel_id,
                        message_id: wire.message_id,
                        emoji_id: wire.emoji.id,
                        emoji_name: wire.emoji.name.unwrap_or_default(),
                        by_me: false,
                    });
                }
            }
            "MESSAGE_REACTION_REMOVE_ALL" => {
                if let Ok(wire) = serde_json::from_value::<WireReactionRemoveAll>(data) {
                    outcome.events.push(BackendEvent::ReactionRemoved {
                        channel_id: wire.channel_id,
                        message_id: wire.message_id,
                        emoji_id: None,
                        emoji_name: "*".to_owned(),
                        by_me: false,
                    });
                }
            }
            "TYPING_START" => {
                if let Ok(wire) = serde_json::from_value::<WireTypingStart>(data) {
                    let display_name = wire
                        .member
                        .as_ref()
                        .and_then(WireGuildMember::display_name)
                        .or_else(|| {
                            self.users_by_id
                                .get(&wire.user_id)
                                .map(|u| u.display_name().to_owned())
                        })
                        .unwrap_or_else(|| format!("user-{}", wire.user_id));
                    outcome.events.push(BackendEvent::TypingStarted {
                        channel_id: wire.channel_id,
                        user_id: wire.user_id,
                        display_name,
                        expires_at_unix: wire.timestamp.saturating_add(10),
                    });
                }
            }
            "PRESENCE_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WirePresence>(data)
                    && let Some(presence) = wire.into_core()
                {
                    if let Some(user) = self.users_by_id.get_mut(&presence.user_id) {
                        user.presence = presence.status;
                    }
                    outcome.events.push(BackendEvent::PresenceUpdated(presence));
                }
            }
            "VOICE_STATE_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireVoiceState>(data) {
                    self.apply_voice_state(wire, outcome);
                }
            }
            "VOICE_SERVER_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireVoiceServerUpdate>(data)
                    && let Ok(token) = SecretToken::new(wire.token)
                {
                    outcome.signaling.push(GatewaySignalingEvent::VoiceServerUpdate {
                        token,
                        guild_id: wire.guild_id,
                        channel_id: wire.channel_id,
                        endpoint: wire.endpoint,
                    });
                }
            }
            "CALL_CREATE" => {
                if let Ok(wire) = serde_json::from_value::<WireCallEvent>(data) {
                    self.handle_call_event(wire, true, outcome);
                }
            }
            "CALL_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireCallEvent>(data) {
                    self.handle_call_event(wire, false, outcome);
                }
            }
            "CALL_DELETE" => {
                if let Ok(wire) = serde_json::from_value::<WireCallDelete>(data) {
                    self.handle_call_delete(wire.channel_id, outcome);
                }
            }
            "STREAM_CREATE" => {
                if let Ok(wire) = serde_json::from_value::<WireStreamCreate>(data) {
                    self.apply_stream_create(wire, outcome);
                }
            }
            "STREAM_SERVER_UPDATE" => {
                if let Ok(wire) = serde_json::from_value::<WireStreamServerUpdate>(data)
                    && let Ok(token) = SecretToken::new(wire.token)
                {
                    outcome.signaling.push(GatewaySignalingEvent::StreamServerUpdate {
                        token,
                        stream_key: wire.stream_key,
                        endpoint: wire.endpoint,
                    });
                }
            }
            "STREAM_DELETE" => {
                if let Ok(wire) = serde_json::from_value::<WireStreamDelete>(data) {
                    let removed = self.active_streams.remove(&wire.stream_key);
                    outcome.signaling.push(GatewaySignalingEvent::StreamDelete {
                        stream_key: wire.stream_key,
                        reason: wire.reason,
                    });
                    if let Some(desc) = removed {
                        self.emit_channel_streams(desc.channel_id, outcome);
                    }
                }
            }
            _ => {
                debug!(event = event_name, "ignoring unhandled gateway dispatch");
            }
        }
    }

    fn handle_ready(&mut self, data: serde_json::Value, outcome: &mut GatewayFrameOutcome) {
        let ready = match serde_json::from_value::<WireReady>(data) {
            Ok(ready) => ready,
            Err(err) => {
                warn!("failed to decode READY dispatch: {err}");
                outcome.events.push(BackendEvent::ErrorBanner {
                    scope: "gateway".to_owned(),
                    message: format!("READY decode failed: {err}"),
                    recoverable: true,
                });
                return;
            }
        };

        self.session_id = Some(ready.session_id);
        self.resume_gateway_url = ready.resume_gateway_url;

        let current_user = User::from(ready.user);
        self.users_by_id
            .insert(current_user.id, current_user.clone());
        self.current_user = Some(current_user.clone());

        for wire_user in ready.users {
            let user = User::from(wire_user);
            self.users_by_id.insert(user.id, user);
        }

        // Parse read states (supports both legacy Vec and versioned { entries: Vec }).
        if let Some(rs_val) = ready.read_state {
            for entry in parse_read_state_value(rs_val) {
                self.read_states.insert(entry.channel_id, entry);
            }
        }

        // Parse relationships.
        for rel in ready.relationships {
            if let Some(user) = rel.user {
                let core_user = User::from(user);
                self.users_by_id.insert(core_user.id, core_user);
            }
            let uid = rel.user_id.or(rel.id);
            if let Some(user_id) = uid {
                self.relationships.insert(
                    user_id,
                    RelationshipEntry {
                        user_id,
                        kind: rel.kind,
                        nickname: rel.nickname,
                    },
                );
            }
        }

        // Parse user_guild_settings (supports both versioned { entries: Vec } and flat Vec).
        let raw_guild_settings: Vec<WireUserGuildSettingsEntry> = ready
            .user_guild_settings
            .map(parse_user_guild_settings_value)
            .unwrap_or_default();
        let mut settings_by_guild: HashMap<Option<Snowflake>, &WireUserGuildSettingsEntry> =
            HashMap::new();
        for entry in &raw_guild_settings {
            for co in &entry.channel_overrides {
                self.muted_channels.insert(co.channel_id, co.muted);
            }
            settings_by_guild.insert(entry.guild_id, entry);
        }

        // Parse private channels (1:1 DMs and Group DMs = type 3).
        let mut dm_channels: Vec<Channel> = ready
            .private_channels
            .into_iter()
            .map(|wc| {
                let mut ch = wc.into_core_channel(None, &self.users_by_id);
                if let Some(&muted) = self.muted_channels.get(&ch.id) {
                    ch.muted = muted;
                }
                if let Some(rs) = self.read_states.get(&ch.id) {
                    ch.last_ack_message_id = rs.last_message_id;
                    ch.mention_count = rs.mention_count;
                    if is_channel_unread(ch.kind, ch.last_message_id, rs.last_message_id) {
                        ch.unread_count = ch.unread_count.max(1);
                    }
                }
                self.channels_by_id.insert(ch.id, ch.clone());
                ch
            })
            .collect();
        dm_channels.sort_by(|a, b| b.last_message_id.cmp(&a.last_message_id).then(b.id.cmp(&a.id)));

        // Parse guilds, roles, channels, initial members, and initial voice rosters.
        let mut guilds = Vec::with_capacity(ready.guilds.len());
        let mut per_guild_channels: Vec<(Snowflake, Vec<Channel>)> = Vec::new();
        let mut per_guild_members: Vec<(Snowflake, Vec<GuildMemberSummary>)> = Vec::new();
        let mut initial_voice_rosters: Vec<(Option<Snowflake>, Snowflake)> = Vec::new();

        for wire_guild in ready.guilds {
            let guild_id = wire_guild.id;
            let mut guild = wire_guild.to_core_guild();
            if let Some(settings) = settings_by_guild.get(&Some(guild_id)) {
                guild.muted = settings.muted;
                guild.suppress_everyone = settings.suppress_everyone;
                guild.suppress_roles = settings.suppress_roles;
            }
            self.guild_roles.insert(guild_id, guild.roles.clone());

            // Remember any initial guild members shipped in READY.
            if !wire_guild.members.is_empty() {
                let mut summaries = Vec::with_capacity(wire_guild.members.len());
                for m in &wire_guild.members {
                    if let Some(ref wu) = m.user {
                        let u = User::from(wu.clone());
                        self.users_by_id.insert(u.id, u);
                    }
                    if let Some(summary) = m.to_member_summary(None) {
                        summaries.push(summary);
                    }
                }
                if !summaries.is_empty() {
                    per_guild_members.push((guild_id, summaries));
                }
            }

            let mut channels: Vec<Channel> = wire_guild
                .channels
                .into_iter()
                .map(|wc| {
                    let mut ch = wc.into_core_channel(Some(guild_id), &self.users_by_id);
                    if let Some(&muted) = self.muted_channels.get(&ch.id) {
                        ch.muted = muted;
                    } else if let Some(parent) = ch.parent_id
                        && self.muted_channels.get(&parent).copied().unwrap_or(false)
                    {
                        ch.muted = true;
                    }
                    if let Some(rs) = self.read_states.get(&ch.id) {
                        ch.last_ack_message_id = rs.last_message_id;
                        ch.mention_count = rs.mention_count;
                        if is_channel_unread(ch.kind, ch.last_message_id, rs.last_message_id) {
                            ch.unread_count = 1;
                            if !guild.muted && !ch.muted {
                                guild.unread = true;
                            }
                        }
                        guild.mention_count = guild.mention_count.saturating_add(rs.mention_count);
                    }
                    self.channels_by_id.insert(ch.id, ch.clone());
                    ch
                })
                .collect();
            channels.sort_by_key(|c| (c.position, c.id));

            if !wire_guild.voice_states.is_empty() {
                let mut touched_channels = BTreeSet::new();
                for vs in &wire_guild.voice_states {
                    if let Some(member) = &vs.member
                        && let Some(wu) = &member.user
                    {
                        self.users_by_id.insert(wu.id, User::from(wu.clone()));
                    }
                    if let Some(ch_id) = vs.channel_id {
                        self.voice_states
                            .entry(ch_id)
                            .or_default()
                            .insert(vs.user_id, vs.clone());
                        touched_channels.insert(ch_id);
                    }
                }
                for ch_id in touched_channels {
                    initial_voice_rosters.push((Some(guild_id), ch_id));
                }
                outcome
                    .signaling
                    .push(GatewaySignalingEvent::InitialVoiceStates {
                        guild_id: Some(guild_id),
                        states: wire_guild.voice_states,
                    });
            }

            guilds.push(guild);
            if !channels.is_empty() {
                per_guild_channels.push((guild_id, channels));
            }
        }

        let guild_folders: Vec<GuildFolder> = ready
            .user_settings
            .map(|s| s.guild_folders.into_iter().map(GuildFolder::from).collect())
            .unwrap_or_default();

        outcome.events.push(BackendEvent::Ready {
            current_user,
            guilds,
            guild_folders,
            dm_channels,
        });
        for (guild_id, channels) in per_guild_channels {
            outcome.events.push(BackendEvent::ChannelsLoaded {
                guild_id: Some(guild_id),
                channels,
            });
        }
        for entry in raw_guild_settings {
            outcome.events.push(BackendEvent::GuildSettingsUpdated {
                guild_id: entry.guild_id,
                muted: entry.muted,
                suppress_everyone: entry.suppress_everyone,
                suppress_roles: entry.suppress_roles,
                muted_channels: entry
                    .channel_overrides
                    .into_iter()
                    .map(|co| (co.channel_id, co.muted))
                    .collect(),
            });
        }
        for (guild_id, members) in per_guild_members {
            outcome.events.push(BackendEvent::GuildMembersUpdated { guild_id, members });
        }
        for (guild_id, channel_id) in initial_voice_rosters {
            let participants = self.build_voice_roster(channel_id);
            outcome.events.push(BackendEvent::VoiceRosterUpdated {
                guild_id,
                channel_id,
                participants,
            });
        }
        outcome.events.push(BackendEvent::ConnectionChanged {
            state: ConnectionState::Ready,
            latency_ms: None,
            detail: None,
        });
    }

    fn apply_guild_settings_entry(
        &mut self,
        entry: WireUserGuildSettingsEntry,
        outcome: &mut GatewayFrameOutcome,
    ) {
        let mut muted_channels = Vec::with_capacity(entry.channel_overrides.len());
        for co in entry.channel_overrides {
            self.muted_channels.insert(co.channel_id, co.muted);
            if let Some(ch) = self.channels_by_id.get_mut(&co.channel_id) {
                ch.muted = co.muted;
            }
            muted_channels.push((co.channel_id, co.muted));
        }
        outcome.events.push(BackendEvent::GuildSettingsUpdated {
            guild_id: entry.guild_id,
            muted: entry.muted,
            suppress_everyone: entry.suppress_everyone,
            suppress_roles: entry.suppress_roles,
            muted_channels,
        });
    }

    fn handle_ready_supplemental(
        &mut self,
        data: serde_json::Value,
        outcome: &mut GatewayFrameOutcome,
    ) {
        let Ok(supp) = serde_json::from_value::<WireReadySupplemental>(data) else {
            warn!("failed to decode READY_SUPPLEMENTAL dispatch");
            return;
        };

        for g in supp.guilds {
            if !g.voice_states.is_empty() {
                let mut touched_channels = BTreeSet::new();
                for vs in &g.voice_states {
                    if let Some(member) = &vs.member
                        && let Some(wu) = &member.user
                    {
                        self.users_by_id.insert(wu.id, User::from(wu.clone()));
                    }
                    if let Some(ch_id) = vs.channel_id {
                        self.voice_states
                            .entry(ch_id)
                            .or_default()
                            .insert(vs.user_id, vs.clone());
                        touched_channels.insert(ch_id);
                    }
                }
                for ch_id in touched_channels {
                    let participants = self.build_voice_roster(ch_id);
                    outcome.events.push(BackendEvent::VoiceRosterUpdated {
                        guild_id: Some(g.id),
                        channel_id: ch_id,
                        participants,
                    });
                }
                outcome
                    .signaling
                    .push(GatewaySignalingEvent::InitialVoiceStates {
                        guild_id: Some(g.id),
                        states: g.voice_states,
                    });
            }
        }

        if let Some(merged) = supp.merged_presences {
            for p in merged.friends {
                if let Some(presence) = p.into_core() {
                    if let Some(u) = self.users_by_id.get_mut(&presence.user_id) {
                        u.presence = presence.status;
                    }
                    outcome.events.push(BackendEvent::PresenceUpdated(presence));
                }
            }
            for guild_presences in merged.guilds {
                for p in guild_presences {
                    if let Some(presence) = p.into_core() {
                        if let Some(u) = self.users_by_id.get_mut(&presence.user_id) {
                            u.presence = presence.status;
                        }
                        outcome.events.push(BackendEvent::PresenceUpdated(presence));
                    }
                }
            }
        }

        for wc in supp.lazy_private_channels {
            let mut ch = wc.into_core_channel(None, &self.users_by_id);
            if let Some(&muted) = self.muted_channels.get(&ch.id) {
                ch.muted = muted;
            }
            self.channels_by_id.insert(ch.id, ch.clone());
            outcome.events.push(BackendEvent::ChannelUpserted(ch));
        }
    }

    fn apply_voice_state(&mut self, vs: WireVoiceState, outcome: &mut GatewayFrameOutcome) {
        // Remove user from any prior voice channel.
        let mut previous_channel: Option<Snowflake> = None;
        for (&ch_id, members) in &mut self.voice_states {
            if members.remove(&vs.user_id).is_some() {
                previous_channel = Some(ch_id);
            }
        }

        if let Some(member) = &vs.member
            && let Some(wire_user) = &member.user
        {
            self.users_by_id
                .insert(wire_user.id, User::from(wire_user.clone()));
        }

        let target_channel = vs.channel_id;
        if let Some(ch_id) = target_channel {
            self.voice_states
                .entry(ch_id)
                .or_default()
                .insert(vs.user_id, vs.clone());
        }

        outcome
            .signaling
            .push(GatewaySignalingEvent::VoiceStateUpdate(vs.clone()));

        // Emit channel-scoped sidebar voice rosters (NEVER `BackendEvent::VoiceStatusUpdated`,
        // which is exclusively owned by `DiscordVoiceEngine`).
        if let Some(prev_ch) = previous_channel
            && Some(prev_ch) != target_channel
        {
            let prev_guild = self
                .channels_by_id
                .get(&prev_ch)
                .and_then(|c| c.guild_id)
                .or(vs.guild_id);
            let participants = self.build_voice_roster(prev_ch);
            outcome.events.push(BackendEvent::VoiceRosterUpdated {
                guild_id: prev_guild,
                channel_id: prev_ch,
                participants,
            });
        }

        if let Some(ch_id) = target_channel {
            let guild_id = vs
                .guild_id
                .or_else(|| self.channels_by_id.get(&ch_id).and_then(|c| c.guild_id));
            let participants = self.build_voice_roster(ch_id);
            outcome.events.push(BackendEvent::VoiceRosterUpdated {
                guild_id,
                channel_id: ch_id,
                participants,
            });
        }
    }

    fn build_voice_roster(&self, channel_id: Snowflake) -> Vec<VoiceParticipant> {
        let mut participants = Vec::new();
        if let Some(states) = self.voice_states.get(&channel_id) {
            for vs in states.values() {
                let display_name = vs
                    .member
                    .as_ref()
                    .and_then(WireGuildMember::display_name)
                    .or_else(|| {
                        self.users_by_id
                            .get(&vs.user_id)
                            .map(|u| u.display_name().to_owned())
                    })
                    .unwrap_or_else(|| format!("user-{}", vs.user_id));

                participants.push(VoiceParticipant {
                    user_id: vs.user_id,
                    display_name,
                    speaking: false,
                    self_mute: vs.self_mute,
                    self_deaf: vs.self_deaf,
                    server_mute: vs.mute,
                    server_deaf: vs.deaf,
                    volume: 100,
                    rms_db: VoiceParticipant::MIN_RMS_DB,
                    dave_verified: false,
                    streaming: vs.self_stream.unwrap_or(false),
                });
            }
        }
        participants.sort_by_key(|p| p.user_id);
        participants
    }

    fn handle_call_event(
        &mut self,
        call: WireCallEvent,
        is_create: bool,
        outcome: &mut GatewayFrameOutcome,
    ) {
        let is_group_dm = self
            .channels_by_id
            .get(&call.channel_id)
            .is_some_and(|ch| ch.kind == ChannelKind::GroupDm);
        let caller_id = call.voice_states.first().map(|vs| vs.user_id);
        let caller_name = caller_id.map(|cid| {
            self.users_by_id
                .get(&cid)
                .map(|u| u.display_name().to_owned())
                .unwrap_or_else(|| format!("user-{cid}"))
        });

        // Update sidebar voice roster for this DM/Group DM call channel.
        if !call.voice_states.is_empty() {
            let entry = self.voice_states.entry(call.channel_id).or_default();
            entry.clear();
            for vs in &call.voice_states {
                if let Some(member) = &vs.member
                    && let Some(wu) = &member.user
                {
                    self.users_by_id.insert(wu.id, User::from(wu.clone()));
                }
                entry.insert(vs.user_id, vs.clone());
            }
            let participants = self.build_voice_roster(call.channel_id);
            outcome.events.push(BackendEvent::VoiceRosterUpdated {
                guild_id: None,
                channel_id: call.channel_id,
                participants,
            });
        }

        let ring_state = self.map_call_event(&call, caller_id, caller_name.as_deref(), is_group_dm);
        let sig = if is_create {
            GatewaySignalingEvent::CallCreate {
                channel_id: call.channel_id,
                ringing: call.ringing,
                voice_states: call.voice_states,
                caller_id,
                caller_name,
                is_group_dm,
            }
        } else {
            GatewaySignalingEvent::CallUpdate {
                channel_id: call.channel_id,
                ringing: call.ringing,
                voice_states: call.voice_states,
                caller_id,
                caller_name,
                is_group_dm,
            }
        };
        outcome.signaling.push(sig);

        if ring_state == CallRingState::Idle {
            self.active_calls.remove(&call.channel_id);
            if self.active_call_channel == Some(call.channel_id) {
                self.active_call_channel = self.active_calls.keys().next().copied();
                let next_state = self
                    .active_call_channel
                    .and_then(|cid| self.active_calls.get(&cid).cloned())
                    .unwrap_or(CallRingState::Idle);
                outcome.events.push(BackendEvent::CallRingUpdated(next_state));
            }
        } else {
            self.active_calls.insert(call.channel_id, ring_state.clone());
            self.active_call_channel = Some(call.channel_id);
            outcome.events.push(BackendEvent::CallRingUpdated(ring_state));
        }
    }

    fn handle_call_delete(&mut self, channel_id: Snowflake, outcome: &mut GatewayFrameOutcome) {
        outcome
            .signaling
            .push(GatewaySignalingEvent::CallDelete { channel_id });

        if self.voice_states.remove(&channel_id).is_some() {
            outcome.events.push(BackendEvent::VoiceRosterUpdated {
                guild_id: None,
                channel_id,
                participants: Vec::new(),
            });
        }

        let removed = self.active_calls.remove(&channel_id).is_some();
        if self.active_call_channel == Some(channel_id) {
            self.active_call_channel = self.active_calls.keys().next().copied();
            let next_state = self
                .active_call_channel
                .and_then(|cid| self.active_calls.get(&cid).cloned())
                .unwrap_or(CallRingState::Idle);
            outcome
                .events
                .push(BackendEvent::CallRingUpdated(next_state));
        } else if !removed && self.active_call_channel.is_none() {
            // No tracked active call; ignore unrelated CALL_DELETE without clobbering anything.
            debug!(%channel_id, "ignoring CALL_DELETE for non-active channel");
        }
    }

    fn map_call_event(
        &self,
        call: &WireCallEvent,
        caller_id: Option<Snowflake>,
        caller_name: Option<&str>,
        is_group_dm: bool,
    ) -> CallRingState {
        let my_id = self.current_user.as_ref().map(|u| u.id);

        if let Some(me) = my_id
            && call.ringing.contains(&me)
        {
            let cid = caller_id.unwrap_or(Snowflake(0));
            let cname = caller_name
                .map(str::to_owned)
                .unwrap_or_else(|| format!("user-{cid}"));
            return CallRingState::IncomingRinging {
                channel_id: call.channel_id,
                caller_id: cid,
                caller_name: cname,
                is_group_dm,
            };
        }

        if !call.ringing.is_empty() {
            return CallRingState::OutgoingRinging {
                channel_id: call.channel_id,
                target_user_ids: call.ringing.clone(),
            };
        }

        let connected_user_ids: Vec<Snowflake> =
            call.voice_states.iter().map(|vs| vs.user_id).collect();
        if !connected_user_ids.is_empty() {
            CallRingState::Active {
                channel_id: call.channel_id,
                connected_user_ids,
            }
        } else {
            CallRingState::Idle
        }
    }

    fn apply_stream_create(&mut self, wire: WireStreamCreate, outcome: &mut GatewayFrameOutcome) {
        let parsed = parse_stream_key(&wire.stream_key);
        let viewer_count = u32::try_from(wire.viewer_ids.len()).unwrap_or(0);

        outcome.signaling.push(GatewaySignalingEvent::StreamCreate {
            stream_key: wire.stream_key.clone(),
            rtc_server_id: wire.rtc_server_id,
            region: wire.region,
            viewer_ids: wire.viewer_ids,
            paused: wire.paused,
        });

        if let Some((guild_id, channel_id, owner_id)) = parsed {
            let owner_name = self
                .users_by_id
                .get(&owner_id)
                .map(|u| u.display_name().to_owned())
                .unwrap_or_else(|| format!("user-{owner_id}"));
            let title = format!("{owner_name}'s Go Live");
            self.active_streams.insert(
                wire.stream_key.clone(),
                StreamDescriptor {
                    stream_key: wire.stream_key,
                    guild_id,
                    channel_id,
                    owner_id,
                    owner_name,
                    title,
                    resolution: (1280, 720),
                    fps: 30,
                    viewer_count,
                    has_audio: true,
                    dave_encrypted: true,
                },
            );
            self.emit_channel_streams(channel_id, outcome);
        }
    }

    fn emit_channel_streams(&self, channel_id: Snowflake, outcome: &mut GatewayFrameOutcome) {
        let mut streams: Vec<StreamDescriptor> = self
            .active_streams
            .values()
            .filter(|s| s.channel_id == channel_id)
            .cloned()
            .collect();
        streams.sort_by(|a, b| a.stream_key.cmp(&b.stream_key));
        outcome.events.push(BackendEvent::ActiveStreamsUpdated {
            channel_id,
            streams,
        });
    }
}

/// Parse a Discord Go Live stream key (`"guild:<guild_id>:<channel_id>:<user_id>"` or
/// `"call:<channel_id>:<user_id>"`) into `(guild_id, channel_id, owner_id)`.
#[must_use]
pub fn parse_stream_key(stream_key: &str) -> Option<(Option<Snowflake>, Snowflake, Snowflake)> {
    let parts: Vec<&str> = stream_key.split(':').collect();
    match parts.as_slice() {
        ["guild", g, c, u] => {
            let guild_id = g.parse::<Snowflake>().ok()?;
            let channel_id = c.parse::<Snowflake>().ok()?;
            let owner_id = u.parse::<Snowflake>().ok()?;
            Some((Some(guild_id), channel_id, owner_id))
        }
        ["call", c, u] => {
            let channel_id = c.parse::<Snowflake>().ok()?;
            let owner_id = u.parse::<Snowflake>().ok()?;
            Some((None, channel_id, owner_id))
        }
        _ => None,
    }
}

/// Control instruction produced when processing a raw Gateway frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayControl {
    /// Server sent `HELLO`; start heartbeat loop and send `IDENTIFY` or `RESUME`.
    Hello {
        /// Heartbeat period in milliseconds.
        heartbeat_interval_ms: u64,
    },
    /// Server sent Op `1`; immediately send a heartbeat frame.
    SendHeartbeatNow,
    /// Server acknowledged our previous heartbeat.
    HeartbeatAck,
    /// Server requested reconnect (`Op 7`).
    Reconnect {
        /// Whether session resume is allowed.
        resumable: bool,
    },
    /// Server rejected `IDENTIFY` or `RESUME` (`Op 9`).
    InvalidSession {
        /// Whether retrying `RESUME` is allowed (`true`) or fresh `IDENTIFY` is needed (`false`).
        resumable: bool,
    },
}

/// Result of handling one decoded Gateway JSON frame.
#[derive(Debug, Default)]
pub struct GatewayFrameOutcome {
    /// Core UI/application events to broadcast.
    pub events: Vec<BackendEvent>,
    /// Low-level voice/video signaling events to broadcast.
    pub signaling: Vec<GatewaySignalingEvent>,
    /// Optional connection control signal.
    pub control: Option<GatewayControl>,
}

/// Asynchronous Discord Gateway v10 driver managing WebSocket lifecycle, zlib-stream inflation,
/// jittered heartbeats, ACK timeout detection, `IDENTIFY`, and `RESUME`.
pub struct DiscordGateway {
    token: SecretToken,
    fingerprint: DiscordFingerprint,
    config: GatewayConfig,
    cmd_rx: mpsc::UnboundedReceiver<GatewayCommand>,
    events_tx: broadcast::Sender<BackendEvent>,
    signaling_tx: broadcast::Sender<GatewaySignalingEvent>,
    state: GatewaySessionState,
}

impl fmt::Debug for DiscordGateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordGateway")
            .field("gateway_url", &self.config.gateway_url)
            .field("capabilities", &self.config.capabilities)
            .field("token", &self.token)
            .finish()
    }
}

impl DiscordGateway {
    /// Create a [`DiscordGateway`] runner and its cloneable [`GatewayHandle`].
    #[must_use]
    pub fn new(
        token: SecretToken,
        fingerprint: DiscordFingerprint,
        config: GatewayConfig,
        events_tx: broadcast::Sender<BackendEvent>,
    ) -> (Self, GatewayHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (signaling_tx, _) = broadcast::channel(256);
        let handle = GatewayHandle {
            cmd_tx,
            events_tx: events_tx.clone(),
            signaling_tx: signaling_tx.clone(),
        };
        let gateway = Self {
            token,
            fingerprint,
            config,
            cmd_rx,
            events_tx,
            signaling_tx,
            state: GatewaySessionState::default(),
        };
        (gateway, handle)
    }

    /// Run the Gateway connection loop until [`GatewayCommand::Shutdown`] or a fatal auth error.
    pub async fn run(mut self) -> CoreResult<()> {
        let ws_client = self.fingerprint.build_websocket_client()?;
        let mut backoff = self.config.initial_reconnect_backoff;

        loop {
            let should_resume =
                self.state.session_id.is_some() && self.state.sequence.is_some();
            let target_url = if should_resume {
                self.state
                    .resume_gateway_url
                    .as_deref()
                    .map(normalize_gateway_url)
                    .unwrap_or_else(|| self.config.gateway_url.clone())
            } else {
                self.config.gateway_url.clone()
            };

            let _ = self.events_tx.send(BackendEvent::ConnectionChanged {
                state: if should_resume {
                    ConnectionState::Resuming
                } else {
                    ConnectionState::Connecting
                },
                latency_ms: None,
                detail: None,
            });

            match self.run_single_session(&ws_client, &target_url).await {
                SessionExit::Shutdown => {
                    let _ = self.events_tx.send(BackendEvent::ConnectionChanged {
                        state: ConnectionState::Disconnected,
                        latency_ms: None,
                        detail: Some("shutdown".to_owned()),
                    });
                    return Ok(());
                }
                SessionExit::Fatal(err) => {
                    let _ = self.events_tx.send(BackendEvent::ConnectionChanged {
                        state: ConnectionState::Disconnected,
                        latency_ms: None,
                        detail: Some(err.to_string()),
                    });
                    return Err(err);
                }
                SessionExit::Reconnect {
                    reset_backoff,
                    min_delay,
                } => {
                    if reset_backoff {
                        backoff = self.config.initial_reconnect_backoff;
                    }
                    let delay = min_delay.map_or(backoff, |d| d.max(backoff));
                    let _ = self.events_tx.send(BackendEvent::ConnectionChanged {
                        state: ConnectionState::Reconnecting,
                        latency_ms: None,
                        detail: Some(format!("reconnecting in {}ms", delay.as_millis())),
                    });

                    // Allow Shutdown to interrupt reconnect sleep immediately.
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        maybe_cmd = self.cmd_rx.recv() => {
                            if matches!(maybe_cmd, None | Some(GatewayCommand::Shutdown)) {
                                return Ok(());
                            }
                        }
                    }

                    backoff = (backoff.saturating_mul(2)).min(self.config.max_reconnect_backoff);
                }
            }
        }
    }

    async fn run_single_session(&mut self, ws_client: &wreq::Client, url: &str) -> SessionExit {
        let ws_resp = match ws_client
            .websocket(url)
            .header(header::ORIGIN, DISCORD_ORIGIN)
            .header(header::USER_AGENT, self.fingerprint.user_agent())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                warn!("gateway connect error: {err}");
                let _ = self.events_tx.send(BackendEvent::ErrorBanner {
                    scope: "gateway".to_owned(),
                    message: compact_gateway_error("gateway connect error", &err),
                    recoverable: true,
                });
                return SessionExit::Reconnect {
                    reset_backoff: false,
                    min_delay: None,
                };
            }
        };

        let ws = match ws_resp.into_websocket().await {
            Ok(ws) => ws,
            Err(err) => {
                warn!("gateway websocket upgrade error: {err}");
                let _ = self.events_tx.send(BackendEvent::ErrorBanner {
                    scope: "gateway".to_owned(),
                    message: compact_gateway_error("gateway upgrade error", &err),
                    recoverable: true,
                });
                return SessionExit::Reconnect {
                    reset_backoff: false,
                    min_delay: None,
                };
            }
        };

        let (mut ws_sink, mut ws_stream) = ws.split();
        let mut inflater = ZlibStreamInflater::default();
        let mut heartbeat_period: Option<Duration> = None;
        let mut next_heartbeat_at: Option<tokio::time::Instant> = None;
        let mut awaiting_ack = false;
        let mut last_heartbeat_sent: Option<Instant> = None;
        let mut established_ready = false;

        loop {
            let next_event = tokio::select! {
                _ = async {
                    if let Some(deadline) = next_heartbeat_at {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => LoopEvent::HeartbeatTick,

                cmd = self.cmd_rx.recv() => LoopEvent::Command(cmd),

                frame = ws_stream.next() => LoopEvent::WsFrame(frame),
            };

            match next_event {
                LoopEvent::HeartbeatTick => {
                    if awaiting_ack {
                        warn!("gateway heartbeat ACK timeout; closing zombie connection to resume");
                        let _ = ws_sink.close().await;
                        return SessionExit::Reconnect {
                            reset_backoff: established_ready,
                            min_delay: None,
                        };
                    }

                    let Ok(hb_json) = build_heartbeat_payload(self.state.sequence) else {
                        return SessionExit::Reconnect {
                            reset_backoff: established_ready,
                            min_delay: None,
                        };
                    };
                    if let Err(err) = ws_sink.send(wreq::Message::text(hb_json)).await {
                        warn!("gateway heartbeat send failed: {err}");
                        return SessionExit::Reconnect {
                            reset_backoff: established_ready,
                            min_delay: None,
                        };
                    }
                    awaiting_ack = true;
                    last_heartbeat_sent = Some(Instant::now());
                    if let Some(period) = heartbeat_period {
                        next_heartbeat_at = Some(tokio::time::Instant::now() + period);
                    }
                }

                LoopEvent::Command(None | Some(GatewayCommand::Shutdown)) => {
                    let _ = ws_sink.close().await;
                    return SessionExit::Shutdown;
                }

                LoopEvent::Command(Some(cmd)) => match serialize_gateway_command(&cmd) {
                    Ok(Some(json)) => {
                        if let Err(err) = ws_sink.send(wreq::Message::text(json)).await {
                            warn!("gateway outbound command send failed: {err}");
                            return SessionExit::Reconnect {
                                reset_backoff: established_ready,
                                min_delay: None,
                            };
                        }
                    }
                    Ok(None) => {
                        let _ = ws_sink.close().await;
                        return SessionExit::Shutdown;
                    }
                    Err(err) => {
                        warn!("failed to serialize gateway command: {err}");
                    }
                },

                LoopEvent::WsFrame(None) => {
                    info!("gateway websocket stream ended; reconnecting");
                    return SessionExit::Reconnect {
                        reset_backoff: established_ready,
                        min_delay: None,
                    };
                }

                LoopEvent::WsFrame(Some(Err(err))) => {
                    warn!("gateway websocket read error: {err}");
                    return SessionExit::Reconnect {
                        reset_backoff: established_ready,
                        min_delay: None,
                    };
                }

                LoopEvent::WsFrame(Some(Ok(msg))) => {
                    let raw_json: Vec<u8> = match msg {
                        wreq::Message::Binary(bin) => match inflater.push_chunk(&bin) {
                            Ok(Some(decompressed)) => decompressed,
                            Ok(None) => continue,
                            Err(err) => {
                                warn!("gateway zlib-stream decode error: {err}");
                                let _ = ws_sink.close().await;
                                return SessionExit::Reconnect {
                                    reset_backoff: false,
                                    min_delay: None,
                                };
                            }
                        },
                        wreq::Message::Text(text) => text.as_bytes().to_vec(),
                        wreq::Message::Ping(payload) => {
                            let _ = ws_sink.send(wreq::Message::Pong(payload)).await;
                            continue;
                        }
                        wreq::Message::Pong(_) => continue,
                        wreq::Message::Close(close_frame) => {
                            let code = close_frame.as_ref().map(|c| c.code.0);
                            let reason = close_frame
                                .as_ref()
                                .map(|c| c.reason.as_str())
                                .unwrap_or("");
                            warn!(close_code = ?code, close_reason = %reason, "gateway websocket closed by remote");
                            let _ = self.events_tx.send(BackendEvent::ErrorBanner {
                                scope: "gateway".to_owned(),
                                message: format!("gateway closed (code={code:?}, reason={reason})"),
                                recoverable: true,
                            });
                            return classify_close_code(code, established_ready, &mut self.state);
                        }
                    };

                    let outcome = match self.state.handle_raw_frame(&raw_json) {
                        Ok(o) => o,
                        Err(err) => {
                            warn!("gateway frame handling error: {err}");
                            continue;
                        }
                    };

                    for ev in outcome.events {
                        if matches!(
                            ev,
                            BackendEvent::Ready { .. }
                                | BackendEvent::ConnectionChanged {
                                    state: ConnectionState::Ready,
                                    ..
                                }
                        ) {
                            established_ready = true;
                        }
                        let _ = self.events_tx.send(ev);
                    }
                    for sig in outcome.signaling {
                        let _ = self.signaling_tx.send(sig);
                    }

                    if let Some(control) = outcome.control {
                        match control {
                            GatewayControl::Hello {
                                heartbeat_interval_ms,
                            } => {
                                let period = Duration::from_millis(heartbeat_interval_ms);
                                let jitter = self
                                    .config
                                    .heartbeat_jitter_override
                                    .unwrap_or_else(|| rand::random::<f64>().clamp(0.05, 0.95));
                                let initial_delay = period.mul_f64(jitter.clamp(0.01, 1.0));
                                heartbeat_period = Some(period);
                                next_heartbeat_at = Some(tokio::time::Instant::now() + initial_delay);
                                awaiting_ack = false;

                                let handshake_json = if let (Some(session_id), Some(seq)) =
                                    (&self.state.session_id, self.state.sequence)
                                {
                                    build_resume_payload(&self.token, session_id, seq)
                                } else {
                                    build_identify_payload(
                                        &self.token,
                                        self.config.capabilities,
                                        self.fingerprint.identify_properties(),
                                    )
                                };

                                match handshake_json {
                                    Ok(secret_payload) => {
                                        let send_res = ws_sink
                                            .send(wreq::Message::text(secret_payload.as_str().to_owned()))
                                            .await;
                                        drop(secret_payload);
                                        if let Err(err) = send_res {
                                            warn!("gateway handshake send failed: {err}");
                                            return SessionExit::Reconnect {
                                                reset_backoff: false,
                                                min_delay: None,
                                            };
                                        }
                                    }
                                    Err(err) => return SessionExit::Fatal(err),
                                }
                            }
                            GatewayControl::SendHeartbeatNow => {
                                if let Ok(hb) = build_heartbeat_payload(self.state.sequence) {
                                    let _ = ws_sink.send(wreq::Message::text(hb)).await;
                                    awaiting_ack = true;
                                    last_heartbeat_sent = Some(Instant::now());
                                }
                            }
                            GatewayControl::HeartbeatAck => {
                                awaiting_ack = false;
                                let latency_ms = last_heartbeat_sent.map(|sent| {
                                    u32::try_from(sent.elapsed().as_millis()).unwrap_or(u32::MAX)
                                });
                                let state = if established_ready {
                                    ConnectionState::Ready
                                } else {
                                    ConnectionState::Connecting
                                };
                                let _ = self.events_tx.send(BackendEvent::ConnectionChanged {
                                    state,
                                    latency_ms,
                                    detail: None,
                                });
                            }
                            GatewayControl::Reconnect { .. } => {
                                let _ = ws_sink.close().await;
                                return SessionExit::Reconnect {
                                    reset_backoff: established_ready,
                                    min_delay: None,
                                };
                            }
                            GatewayControl::InvalidSession { resumable } => {
                                let _ = ws_sink.close().await;
                                let min_delay = if resumable {
                                    None
                                } else {
                                    Some(Duration::from_millis(1_500))
                                };
                                return SessionExit::Reconnect {
                                    reset_backoff: false,
                                    min_delay,
                                };
                            }
                        }
                    }
                }
            }
        }
    }
}

enum LoopEvent {
    HeartbeatTick,
    Command(Option<GatewayCommand>),
    WsFrame(Option<Result<wreq::Message, wreq::Error>>),
}

enum SessionExit {
    Shutdown,
    Fatal(CoreError),
    Reconnect {
        reset_backoff: bool,
        min_delay: Option<Duration>,
    },
}

fn compact_gateway_error(prefix: &str, err: &dyn fmt::Display) -> String {
    let raw = err.to_string();
    let single_line: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let clipped: String = single_line.chars().take(120).collect();
    if single_line.chars().count() > 120 {
        format!("{prefix}: {clipped}…")
    } else {
        format!("{prefix}: {clipped}")
    }
}

fn classify_close_code(
    code: Option<u16>,
    established_ready: bool,
    state: &mut GatewaySessionState,
) -> SessionExit {
    match code {
        Some(4004) => SessionExit::Fatal(CoreError::AuthRequired),
        Some(4010..=4014) => SessionExit::Fatal(CoreError::Protocol(
            format!("fatal gateway close code {}", code.unwrap_or(0)),
        )),
        // 4007 (Invalid seq) or 4009 (Session timed out) require fresh IDENTIFY.
        Some(4007 | 4009) => {
            state.session_id = None;
            state.resume_gateway_url = None;
            state.sequence = None;
            SessionExit::Reconnect {
                reset_backoff: established_ready,
                min_delay: None,
            }
        }
        _ => SessionExit::Reconnect {
            reset_backoff: established_ready,
            min_delay: None,
        },
    }
}

// ============================================================================
// Private Gateway Wire Payloads
// ============================================================================

#[derive(Debug, Deserialize)]
struct WireGatewayPayload {
    op: u8,
    #[serde(default)]
    d: Option<serde_json::Value>,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireHello {
    heartbeat_interval: u64,
}

fn is_channel_unread(
    kind: ChannelKind,
    last_message_id: Option<Snowflake>,
    ack_message_id: Option<Snowflake>,
) -> bool {
    if kind.is_voice() || kind == ChannelKind::GuildCategory {
        return false;
    }
    match (last_message_id, ack_message_id) {
        (Some(last), Some(ack)) => last > ack,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[derive(Debug, Deserialize)]
struct WireReady {
    session_id: String,
    #[serde(default)]
    resume_gateway_url: Option<String>,
    user: WireUser,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    users: Vec<WireUser>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    guilds: Vec<WireGuild>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    private_channels: Vec<WireChannel>,
    #[serde(default)]
    user_settings: Option<WireReadyUserSettings>,
    #[serde(default)]
    user_guild_settings: Option<serde_json::Value>,
    #[serde(default)]
    read_state: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    relationships: Vec<WireRelationship>,
}

#[derive(Debug, Deserialize)]
struct WireChannelOverride {
    channel_id: Snowflake,
    #[serde(default)]
    muted: bool,
}

#[derive(Debug, Deserialize)]
struct WireUserGuildSettingsEntry {
    #[serde(default)]
    guild_id: Option<Snowflake>,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    suppress_everyone: bool,
    #[serde(default)]
    suppress_roles: bool,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    channel_overrides: Vec<WireChannelOverride>,
}

#[derive(Debug, Deserialize)]
struct WireVersionedUserGuildSettings {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    entries: Vec<WireUserGuildSettingsEntry>,
}

fn parse_user_guild_settings_value(val: serde_json::Value) -> Vec<WireUserGuildSettingsEntry> {
    if let Ok(entries) = serde_json::from_value::<Vec<WireUserGuildSettingsEntry>>(val.clone()) {
        return entries;
    }
    if let Ok(versioned) = serde_json::from_value::<WireVersionedUserGuildSettings>(val) {
        return versioned.entries;
    }
    Vec::new()
}

#[derive(Debug, Deserialize)]
struct WireMessageAck {
    channel_id: Snowflake,
    #[serde(default)]
    message_id: Option<Snowflake>,
    #[serde(default)]
    mention_count: u32,
}

#[derive(Debug, Deserialize)]
struct WireGuildRoleUpsert {
    guild_id: Snowflake,
    role: WireRole,
}

#[derive(Debug, Deserialize)]
struct WireGuildRoleDelete {
    guild_id: Snowflake,
    role_id: Snowflake,
}

#[derive(Debug, Deserialize)]
struct WireGuildMemberUpdate {
    guild_id: Snowflake,
    user: WireUser,
    #[serde(default)]
    nick: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    roles: Vec<Snowflake>,
}

#[derive(Debug, Deserialize)]
struct WireGuildMembersChunk {
    guild_id: Snowflake,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    members: Vec<WireGuildMember>,
}

#[derive(Debug, Deserialize)]
struct WireGuildMemberListUpdate {
    guild_id: Snowflake,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    ops: Vec<WireMemberListOp>,
}

#[derive(Debug, Deserialize)]
struct WireMemberListOp {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    items: Vec<WireMemberListItem>,
    #[serde(default)]
    item: Option<WireMemberListItem>,
}

#[derive(Debug, Deserialize)]
struct WireMemberListItem {
    #[serde(default)]
    member: Option<WireMemberListMember>,
}

#[derive(Debug, Deserialize)]
struct WireMemberListMember {
    #[serde(flatten)]
    member: WireGuildMember,
    #[serde(default)]
    presence: Option<WirePresence>,
}

#[derive(Debug, Deserialize)]
struct WireReadyUserSettings {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    guild_folders: Vec<WireGuildFolder>,
}

#[derive(Debug, Deserialize)]
struct WireRelationship {
    #[serde(default)]
    id: Option<Snowflake>,
    #[serde(default)]
    user_id: Option<Snowflake>,
    #[serde(rename = "type", default)]
    kind: u8,
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    user: Option<WireUser>,
}

#[derive(Debug, Deserialize)]
struct WireReadStateEntry {
    id: Snowflake,
    #[serde(default)]
    last_message_id: Option<Snowflake>,
    #[serde(default)]
    mention_count: u32,
}

#[derive(Debug, Deserialize)]
struct WireVersionedReadState {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    entries: Vec<WireReadStateEntry>,
}

fn parse_read_state_value(val: serde_json::Value) -> Vec<ReadStateEntry> {
    if let Ok(entries) = serde_json::from_value::<Vec<WireReadStateEntry>>(val.clone()) {
        return entries
            .into_iter()
            .map(|e| ReadStateEntry {
                channel_id: e.id,
                last_message_id: e.last_message_id,
                mention_count: e.mention_count,
            })
            .collect();
    }
    if let Ok(versioned) = serde_json::from_value::<WireVersionedReadState>(val) {
        return versioned
            .entries
            .into_iter()
            .map(|e| ReadStateEntry {
                channel_id: e.id,
                last_message_id: e.last_message_id,
                mention_count: e.mention_count,
            })
            .collect();
    }
    Vec::new()
}

#[derive(Debug, Deserialize)]
struct WireReadySupplemental {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    guilds: Vec<WireSupplementalGuild>,
    #[serde(default)]
    merged_presences: Option<WireMergedPresences>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    lazy_private_channels: Vec<WireChannel>,
}

#[derive(Debug, Deserialize)]
struct WireSupplementalGuild {
    id: Snowflake,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    voice_states: Vec<WireVoiceState>,
}

#[derive(Debug, Deserialize)]
struct WireMergedPresences {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    friends: Vec<WirePresence>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    guilds: Vec<Vec<WirePresence>>,
}

#[derive(Debug, Deserialize)]
struct WireChannelDelete {
    id: Snowflake,
    #[serde(default)]
    guild_id: Option<Snowflake>,
}

#[derive(Debug, Deserialize)]
struct WireChannelRecipientEvent {
    channel_id: Snowflake,
    user: WireUser,
}

#[derive(Debug, Deserialize)]
struct WireMessageDelete {
    id: Snowflake,
    channel_id: Snowflake,
}

#[derive(Debug, Deserialize)]
struct WireReactionAdd {
    user_id: Snowflake,
    channel_id: Snowflake,
    message_id: Snowflake,
    emoji: WireEmoji,
}

#[derive(Debug, Deserialize)]
struct WireReactionAddMany {
    channel_id: Snowflake,
    message_id: Snowflake,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    reactions: Vec<WireReactionBurstEntry>,
}

#[derive(Debug, Deserialize)]
struct WireReactionBurstEntry {
    emoji: WireEmoji,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    users: Vec<Snowflake>,
}

#[derive(Debug, Deserialize)]
struct WireReactionRemove {
    user_id: Snowflake,
    channel_id: Snowflake,
    message_id: Snowflake,
    emoji: WireEmoji,
}

#[derive(Debug, Deserialize)]
struct WireReactionRemoveEmoji {
    channel_id: Snowflake,
    message_id: Snowflake,
    emoji: WireEmoji,
}

#[derive(Debug, Deserialize)]
struct WireReactionRemoveAll {
    channel_id: Snowflake,
    message_id: Snowflake,
}

#[derive(Debug, Deserialize)]
struct WireTypingStart {
    channel_id: Snowflake,
    user_id: Snowflake,
    timestamp: i64,
    #[serde(default)]
    member: Option<WireGuildMember>,
}

#[derive(Debug, Deserialize)]
struct WireVoiceServerUpdate {
    token: String,
    #[serde(default)]
    guild_id: Option<Snowflake>,
    #[serde(default)]
    channel_id: Option<Snowflake>,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireCallEvent {
    channel_id: Snowflake,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    ringing: Vec<Snowflake>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    voice_states: Vec<WireVoiceState>,
}

#[derive(Debug, Deserialize)]
struct WireCallDelete {
    channel_id: Snowflake,
}

#[derive(Debug, Deserialize)]
struct WireStreamCreate {
    stream_key: String,
    #[serde(default)]
    rtc_server_id: String,
    #[serde(default)]
    region: Option<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    viewer_ids: Vec<Snowflake>,
    #[serde(default)]
    paused: bool,
}

#[derive(Debug, Deserialize)]
struct WireStreamServerUpdate {
    token: String,
    stream_key: String,
    #[serde(default)]
    endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireStreamDelete {
    stream_key: String,
    #[serde(default)]
    reason: Option<String>,
}
