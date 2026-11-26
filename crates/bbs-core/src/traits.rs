//! Asynchronous service trait contracts implemented by protocol, GIF, voice, video, and mock crates.

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::error::CoreResult;
use crate::events::BackendEvent;
use crate::model::{
    Attachment, CallRingState, Channel, DaveSessionState, GifProviderKind, GifSearchPage,
    GifSearchQuery, Guild, GuildFolder, Message, RefreshedAttachmentUrl, Snowflake,
    StreamDescriptor, StreamQuality, StreamSourceKind, User, VideoFrame, VideoSinkKind,
    VoiceChannelStatus, VoiceParticipant, VoicePlaybackState,
};

/// Core Discord REST + Gateway backend contract (guilds, channels, messages, Group DMs, calls, CDN).
#[async_trait]
pub trait DiscordBackend: Send + Sync {
    /// Return the currently authenticated Discord user.
    async fn current_user(&self) -> CoreResult<User>;

    /// Fetch joined guilds and sidebar folder structure.
    async fn list_guilds(&self) -> CoreResult<(Vec<Guild>, Vec<GuildFolder>)>;

    /// Fetch channels belonging to `guild_id`.
    async fn list_channels(&self, guild_id: Snowflake) -> CoreResult<Vec<Channel>>;

    /// Fetch 1:1 DM and Group DM (`type = 3`) channels.
    async fn list_dm_channels(&self) -> CoreResult<Vec<Channel>>;

    /// Fetch chronological messages for `channel_id`, optionally paginating older than `before`.
    async fn fetch_messages(
        &self,
        channel_id: Snowflake,
        before: Option<Snowflake>,
        limit: u8,
    ) -> CoreResult<Vec<Message>>;

    /// Send a message into `channel_id`, optionally replying to `reply_to`.
    async fn send_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
    ) -> CoreResult<Message>;

    /// Edit an existing message authored by the signed-in user.
    async fn edit_message(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        content: &str,
    ) -> CoreResult<Message>;

    /// Delete a message from `channel_id`.
    async fn delete_message(&self, channel_id: Snowflake, message_id: Snowflake) -> CoreResult<()>;

    /// Add an emoji reaction (`"🔥"` or `"custom_name:12345"`) to a message.
    async fn add_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> CoreResult<()>;

    /// Remove the current user's emoji reaction from a message.
    async fn remove_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> CoreResult<()>;

    /// Trigger a typing indicator in `channel_id`.
    async fn send_typing(&self, channel_id: Snowflake) -> CoreResult<()>;

    /// Create a new Group DM (`ChannelKind::GroupDm`) with the specified recipient IDs.
    async fn create_group_dm(
        &self,
        recipient_ids: &[Snowflake],
        name: Option<&str>,
    ) -> CoreResult<Channel>;

    /// Add a recipient to an existing Group DM.
    async fn add_group_dm_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> CoreResult<Channel>;

    /// Remove a recipient from an existing Group DM.
    async fn remove_group_dm_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> CoreResult<Channel>;

    /// Rename an existing Group DM.
    async fn rename_group_dm(&self, channel_id: Snowflake, name: &str) -> CoreResult<Channel>;

    /// Leave (close) a Group DM channel.
    async fn leave_group_dm(&self, channel_id: Snowflake) -> CoreResult<()>;

    /// Ring participants in a 1:1 DM or Group DM voice call.
    async fn ring_call(
        &self,
        channel_id: Snowflake,
        recipient_ids: Option<&[Snowflake]>,
    ) -> CoreResult<CallRingState>;

    /// Stop ringing an outgoing 1:1 DM or Group DM call.
    async fn stop_call_ring(&self, channel_id: Snowflake) -> CoreResult<CallRingState>;

    /// Refresh an expired signed Discord CDN attachment URL.
    async fn refresh_attachment_url(&self, original_url: &str)
    -> CoreResult<RefreshedAttachmentUrl>;

    /// Subscribe to real-time [`BackendEvent`] notifications from this backend.
    fn subscribe_events(&self) -> broadcast::Receiver<BackendEvent>;
}

/// Pluggable GIF search and trending catalog provider (Discord proxy, Tenor, Giphy, Klipy, Mock).
#[async_trait]
pub trait GifProvider: Send + Sync {
    /// Identify which provider backs this instance.
    fn provider_kind(&self) -> GifProviderKind;

    /// Search for GIFs matching `query`.
    async fn search(&self, query: &GifSearchQuery) -> CoreResult<GifSearchPage>;

    /// Fetch currently trending GIFs.
    async fn trending(&self, limit: u8, cursor: Option<&str>) -> CoreResult<GifSearchPage>;
}

/// Voice & audio engine covering guild voice channels, 1:1/Group DM calls, DAVE E2EE, and voice messages.
#[async_trait]
pub trait VoiceEngine: Send + Sync {
    /// Join a guild voice channel (`guild_id = Some(...)`) or a 1:1 / Group DM call (`guild_id = None`).
    async fn join_voice(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    ) -> CoreResult<VoiceChannelStatus>;

    /// Disconnect from the active voice channel or call.
    async fn leave_voice(&self) -> CoreResult<()>;

    /// Mute or unmute local microphone transmission.
    async fn set_self_mute(&self, muted: bool) -> CoreResult<VoiceChannelStatus>;

    /// Deafen or undeafen local audio output (typically also mutes microphone).
    async fn set_self_deaf(&self, deafened: bool) -> CoreResult<VoiceChannelStatus>;

    /// Set local gain (`0..=200`) for a specific voice participant.
    async fn set_participant_volume(
        &self,
        user_id: Snowflake,
        volume: u8,
    ) -> CoreResult<VoiceParticipant>;

    /// Query the current voice session snapshot, if connected.
    async fn status(&self) -> CoreResult<Option<VoiceChannelStatus>>;

    /// Query the current DAVE E2EE / MLS session state.
    async fn dave_state(&self) -> CoreResult<DaveSessionState>;

    /// Start playback of an inline Discord voice message attachment (Ogg/Opus).
    async fn play_voice_message(
        &self,
        message_id: Snowflake,
        attachment: &Attachment,
    ) -> CoreResult<VoicePlaybackState>;

    /// Stop active inline voice message playback.
    async fn stop_voice_message(&self) -> CoreResult<()>;

    /// Subscribe to voice status, Braille VU audio levels, and playback events.
    fn subscribe_voice_events(&self) -> broadcast::Receiver<BackendEvent>;
}

/// Go Live screenshare engine covering both watching remote streams and broadcasting local capture.
#[async_trait]
pub trait VideoStreamEngine: Send + Sync {
    /// List active Go Live screenshare streams in `channel_id`.
    async fn list_streams(&self, channel_id: Snowflake) -> CoreResult<Vec<StreamDescriptor>>;

    /// Begin watching a remote Go Live stream via either a pop-out window or inline terminal sink.
    async fn watch_stream(
        &self,
        stream_key: &str,
        sink: VideoSinkKind,
    ) -> CoreResult<StreamDescriptor>;

    /// Stop watching an active Go Live stream.
    async fn stop_watching(&self, stream_key: &str) -> CoreResult<()>;

    /// Begin broadcasting a local screen/window stream to `channel_id`.
    async fn start_broadcast(
        &self,
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        source: StreamSourceKind,
        quality: StreamQuality,
    ) -> CoreResult<StreamDescriptor>;

    /// Stop the local outbound Go Live broadcast.
    async fn stop_broadcast(&self) -> CoreResult<()>;

    /// Return the currently active outbound broadcast descriptor, if any.
    async fn active_broadcast(&self) -> CoreResult<Option<StreamDescriptor>>;

    /// Subscribe to decoded RGBA8 [`VideoFrame`]s emitted for [`VideoSinkKind::InlineTerminal`] sinks.
    fn subscribe_frames(&self) -> broadcast::Receiver<VideoFrame>;
}
