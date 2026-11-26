//! Unified asynchronous event bus (`BackendEvent`) and UI intent (`UiAction`) contracts.

use serde::{Deserialize, Serialize};

use crate::model::{
    Attachment, CallRingState, Channel, GifItem, GifSearchPage, GifSearchQuery, Guild, GuildFolder,
    Message, Presence, Reaction, Snowflake, StreamDescriptor, StreamQuality, StreamSourceKind,
    User, VideoFrame, VideoSinkKind, VoiceChannelStatus, VoicePlaybackState,
};
use crate::viewport::ViewportGeometry;

/// High-level Gateway transport connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ConnectionState {
    /// Not connected.
    #[default]
    Disconnected,
    /// Dialing Gateway WebSocket / authenticating.
    Connecting,
    /// Authenticated and receiving live dispatch events.
    Ready,
    /// Resuming an existing Gateway session after a transient drop.
    Resuming,
    /// Reconnecting with exponential backoff.
    Reconnecting,
}

/// Events emitted by Discord protocol, GIF, Voice, and Video backends toward the UI layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BackendEvent {
    /// Gateway connectivity or heartbeat latency changed.
    ConnectionChanged {
        /// New connection state.
        state: ConnectionState,
        /// Latest Gateway heartbeat round-trip latency in milliseconds.
        latency_ms: Option<u32>,
        /// Optional diagnostic or BBS status line text.
        detail: Option<String>,
    },
    /// Initial `READY` snapshot containing the signed-in user, guilds, folders, and DMs/Group DMs.
    Ready {
        /// Authenticated user.
        current_user: User,
        /// Joined guilds.
        guilds: Vec<Guild>,
        /// Sidebar guild folder layout.
        guild_folders: Vec<GuildFolder>,
        /// Private 1:1 DM and Group DM channels.
        dm_channels: Vec<Channel>,
    },
    /// Channel list loaded for a guild (or `None` for DM/Group DM home view).
    ChannelsLoaded {
        /// Target guild Snowflake (`None` for DMs/Group DMs).
        guild_id: Option<Snowflake>,
        /// Ordered channels.
        channels: Vec<Channel>,
    },
    /// A channel (including newly created/renamed Group DMs or threads) was created or updated.
    ChannelUpserted(Channel),
    /// A channel was deleted or left (e.g. leaving a Group DM).
    ChannelDeleted {
        /// Owning guild Snowflake (`None` for DMs/Group DMs).
        guild_id: Option<Snowflake>,
        /// Deleted channel Snowflake.
        channel_id: Snowflake,
    },
    /// Historical page of messages fetched for a channel.
    MessagesLoaded {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Messages ordered chronologically (oldest -> newest).
        messages: Vec<Message>,
        /// `true` if older history can still be paginated before the first message.
        has_more_before: bool,
    },
    /// New real-time message arrived (or local send acknowledged).
    MessageCreated(Message),
    /// Existing message content, embeds, or pin state changed.
    MessageUpdated(Message),
    /// Existing message was deleted.
    MessageDeleted {
        /// Channel Snowflake.
        channel_id: Snowflake,
        /// Deleted message Snowflake.
        message_id: Snowflake,
    },
    /// Reaction added to a message.
    ReactionAdded {
        /// Channel Snowflake.
        channel_id: Snowflake,
        /// Message Snowflake.
        message_id: Snowflake,
        /// Updated or added reaction chip.
        reaction: Reaction,
    },
    /// Reaction removed from a message.
    ReactionRemoved {
        /// Channel Snowflake.
        channel_id: Snowflake,
        /// Message Snowflake.
        message_id: Snowflake,
        /// Custom emoji ID when applicable.
        emoji_id: Option<Snowflake>,
        /// Emoji name or Unicode glyph.
        emoji_name: String,
        /// Whether the removed reaction belonged to the current user.
        by_me: bool,
    },
    /// Remote user began typing in a channel.
    TypingStarted {
        /// Channel Snowflake.
        channel_id: Snowflake,
        /// Typing user Snowflake.
        user_id: Snowflake,
        /// Display name for the status bar.
        display_name: String,
        /// Unix seconds when this typing indicator expires.
        expires_at_unix: i64,
    },
    /// User presence status or activity updated.
    PresenceUpdated(Presence),
    /// Asynchronous GIF search or trending response matching a `UiAction::SearchGifs` request.
    GifSearchResults {
        /// Correlation ID issued by the UI picker.
        request_id: u64,
        /// Paginated GIF results.
        page: GifSearchPage,
    },
    /// Full snapshot update of the active voice session (Guild voice or DM/Group DM call).
    VoiceStatusUpdated(VoiceChannelStatus),
    /// High-frequency RMS dBFS and speaking updates for smooth Braille VU meters.
    VoiceAudioLevels {
        /// Active voice/call channel Snowflake.
        channel_id: Snowflake,
        /// Tuples of `(user_id, rms_db, speaking)`.
        levels: Vec<(Snowflake, f32, bool)>,
    },
    /// Inline voice-message attachment playback progress changed.
    VoicePlaybackUpdated(VoicePlaybackState),
    /// 1:1 DM or Group DM call ring state changed.
    CallRingUpdated(CallRingState),
    /// Active Go Live screenshare list updated for a voice/call channel.
    ActiveStreamsUpdated {
        /// Voice or call channel Snowflake.
        channel_id: Snowflake,
        /// Active streams currently available to watch.
        streams: Vec<StreamDescriptor>,
    },
    /// Decoded video frame for an inline terminal stream sink.
    VideoFrameReceived(VideoFrame),
    /// Expired signed Discord CDN attachment URL was refreshed.
    AttachmentUrlRefreshed {
        /// Channel Snowflake.
        channel_id: Snowflake,
        /// Message Snowflake.
        message_id: Snowflake,
        /// Attachment Snowflake.
        attachment_id: Snowflake,
        /// Fresh signed CDN URL.
        url: String,
        /// New expiry timestamp in Unix seconds.
        expires_at_unix: Option<i64>,
    },
    /// User-visible non-fatal error or warning banner.
    ErrorBanner {
        /// Subsystem scope (e.g. `"gateway"`, `"voice"`, `"video"`, `"gif"`).
        scope: String,
        /// Human-readable sanitized message (never contains secrets).
        message: String,
        /// Whether the client can recover automatically or retry.
        recoverable: bool,
    },
}

/// User-initiated actions emitted by the TUI toward core controllers and backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiAction {
    /// Switch active guild (`None` selects the Home / DMs & Group DMs deck).
    SelectGuild(Option<Snowflake>),
    /// Switch active text/DM/Group DM channel.
    SelectChannel(Snowflake),
    /// Fetch older history before `before` message ID.
    LoadOlderMessages {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Oldest currently loaded message Snowflake.
        before: Snowflake,
        /// Page size (`1..=100`).
        limit: u8,
    },
    /// Send a text message (optionally replying to another message or attaching local files).
    SendMessage {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Message content.
        content: String,
        /// Optional message Snowflake being replied to.
        reply_to: Option<Snowflake>,
        /// Optional local filesystem paths to upload as attachments.
        attachment_paths: Vec<String>,
    },
    /// Edit an existing message authored by the current user.
    EditMessage {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Message Snowflake to edit.
        message_id: Snowflake,
        /// Replacement content.
        content: String,
    },
    /// Delete a message.
    DeleteMessage {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Message Snowflake to delete.
        message_id: Snowflake,
    },
    /// Add or remove an emoji reaction on a message.
    ToggleReaction {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Target message Snowflake.
        message_id: Snowflake,
        /// Emoji glyph or `name:id` identifier.
        emoji: String,
        /// `true` to add, `false` to remove.
        add: bool,
    },
    /// Create a new Group DM (`ChannelKind::GroupDm`).
    CreateGroupDm {
        /// Initial recipient user Snowflakes.
        recipient_ids: Vec<Snowflake>,
        /// Optional custom Group DM name.
        name: Option<String>,
    },
    /// Add a user to an existing Group DM.
    AddGroupDmRecipient {
        /// Group DM channel Snowflake.
        channel_id: Snowflake,
        /// User Snowflake to add.
        user_id: Snowflake,
    },
    /// Remove a user from an existing Group DM.
    RemoveGroupDmRecipient {
        /// Group DM channel Snowflake.
        channel_id: Snowflake,
        /// User Snowflake to remove.
        user_id: Snowflake,
    },
    /// Rename an existing Group DM.
    RenameGroupDm {
        /// Group DM channel Snowflake.
        channel_id: Snowflake,
        /// New Group DM name.
        name: String,
    },
    /// Leave a Group DM channel.
    LeaveGroupDm {
        /// Group DM channel Snowflake.
        channel_id: Snowflake,
    },
    /// Ring recipients in a 1:1 DM or Group DM call.
    RingCall {
        /// DM or Group DM channel Snowflake.
        channel_id: Snowflake,
        /// Specific subset of recipients to ring (`None` rings all recipients).
        recipient_ids: Option<Vec<Snowflake>>,
    },
    /// Stop ringing an active outgoing DM/Group DM call.
    StopCallRing {
        /// DM or Group DM channel Snowflake.
        channel_id: Snowflake,
    },
    /// Execute a GIF search or trending fetch.
    SearchGifs {
        /// Correlation ID echoed back in [`BackendEvent::GifSearchResults`].
        request_id: u64,
        /// Query parameters.
        query: GifSearchQuery,
    },
    /// Send a selected GIF into a channel.
    SendGif {
        /// Target channel Snowflake.
        channel_id: Snowflake,
        /// Selected GIF item.
        gif: GifItem,
        /// Optional reply target message Snowflake.
        reply_to: Option<Snowflake>,
    },
    /// Join a guild voice channel or answer/join a 1:1 / Group DM call.
    JoinVoice {
        /// Guild Snowflake (`None` for DM/Group DM calls).
        guild_id: Option<Snowflake>,
        /// Voice or DM/Group DM channel Snowflake.
        channel_id: Snowflake,
    },
    /// Disconnect from the active voice session.
    LeaveVoice,
    /// Toggle local microphone mute.
    SetSelfMute(bool),
    /// Toggle local output deafen.
    SetSelfDeaf(bool),
    /// Adjust per-participant local gain (`0..=200`).
    SetParticipantVolume {
        /// Target participant user Snowflake.
        user_id: Snowflake,
        /// Desired gain percentage (`0..=200`).
        volume: u8,
    },
    /// Play an inline Discord voice message attachment.
    PlayVoiceMessage {
        /// Owning message Snowflake.
        message_id: Snowflake,
        /// Voice message attachment metadata.
        attachment: Attachment,
    },
    /// Stop active inline voice message playback.
    StopVoiceMessage,
    /// Start watching a Go Live screenshare using either a pop-out window or inline terminal sink.
    WatchStream {
        /// Canonical Go Live stream key.
        stream_key: String,
        /// Preferred output sink.
        sink: VideoSinkKind,
    },
    /// Stop watching a Go Live screenshare.
    StopWatchingStream {
        /// Canonical Go Live stream key.
        stream_key: String,
    },
    /// Start broadcasting a local Go Live screenshare.
    StartBroadcast {
        /// Owning guild Snowflake (`None` for DM/Group DM calls).
        guild_id: Option<Snowflake>,
        /// Voice or call channel Snowflake.
        channel_id: Snowflake,
        /// Screen capture source.
        source: StreamSourceKind,
        /// Requested resolution/framerate/bitrate profile.
        quality: StreamQuality,
    },
    /// Stop active outbound Go Live screenshare broadcast.
    StopBroadcast,
    /// Request a fresh signed CDN URL for an expired attachment.
    RefreshAttachmentUrl {
        /// Channel Snowflake.
        channel_id: Snowflake,
        /// Message Snowflake.
        message_id: Snowflake,
        /// Attachment Snowflake.
        attachment_id: Snowflake,
        /// Expired original CDN URL.
        original_url: String,
    },
    /// Live-switch the active BBS/btop theme by name.
    SwitchTheme(String),
    /// Notify controllers that the terminal viewport resized.
    ViewportResized(ViewportGeometry),
    /// Gracefully shut down `bbscord`.
    Quit,
}
