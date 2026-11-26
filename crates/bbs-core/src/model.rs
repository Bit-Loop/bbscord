//! Core Discord, GIF, Voice/DAVE, Call, and Go Live Video domain models.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::error::CoreError;

/// Discord Snowflake 64-bit unique identifier.
///
/// Serializes as a decimal string to preserve precision across JSON boundaries, and accepts
/// either a JSON string or unsigned integer during deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Snowflake(pub u64);

impl Snowflake {
    /// Discord epoch (`2015-01-01T00:00:00.000Z`) in Unix milliseconds.
    pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

    /// Construct a [`Snowflake`] from a raw `u64`.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Return the underlying `u64` value.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Extract the creation timestamp in Unix milliseconds encoded in the high 42 bits.
    #[must_use]
    pub const fn timestamp_ms(self) -> u64 {
        (self.0 >> 22) + Self::DISCORD_EPOCH_MS
    }
}

impl From<u64> for Snowflake {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Snowflake> for u64 {
    fn from(value: Snowflake) -> Self {
        value.0
    }
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Snowflake {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .parse::<u64>()
            .map(Self)
            .map_err(|_| CoreError::InvalidSnowflake(s.to_owned()))
    }
}

impl Serialize for Snowflake {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SnowflakeVisitor;

        impl de::Visitor<'_> for SnowflakeVisitor {
            type Value = Snowflake;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Discord snowflake as a decimal string or u64")
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(Snowflake(v))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                v.parse::<u64>().map(Snowflake).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(SnowflakeVisitor)
    }
}

/// Online presence state for a Discord user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PresenceStatus {
    /// Active and online.
    Online,
    /// Away / idle.
    Idle,
    /// Do Not Disturb.
    DoNotDisturb,
    /// Appearing invisible (self only).
    Invisible,
    /// Offline or unknown.
    #[default]
    Offline,
}

impl PresenceStatus {
    /// Short single-cell Unicode indicator.
    #[must_use]
    pub const fn utf8_symbol(self) -> &'static str {
        match self {
            Self::Online => "●",
            Self::Idle => "◐",
            Self::DoNotDisturb => "⊘",
            Self::Invisible | Self::Offline => "○",
        }
    }

    /// Strict 7-bit ASCII presence indicator.
    #[must_use]
    pub const fn ascii_symbol(self) -> &'static str {
        match self {
            Self::Online => "*",
            Self::Idle => "~",
            Self::DoNotDisturb => "!",
            Self::Invisible | Self::Offline => ".",
        }
    }
}

/// Discord rich activity kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ActivityKind {
    /// Playing a game/app.
    #[default]
    Playing,
    /// Live streaming.
    Streaming,
    /// Listening to music/audio.
    Listening,
    /// Watching media.
    Watching,
    /// Custom user status.
    Custom,
    /// Competing in a match.
    Competing,
}

/// User activity entry attached to a [`Presence`] update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// Activity category.
    pub kind: ActivityKind,
    /// Primary activity name (e.g. `"Neovim"`, `"Spotify"`).
    pub name: String,
    /// Optional detail line.
    pub details: Option<String>,
    /// Optional secondary state line.
    pub state: Option<String>,
}

/// Live presence payload for a Discord user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    /// Target user ID.
    pub user_id: Snowflake,
    /// Online/idle/DND/offline state.
    pub status: PresenceStatus,
    /// Optional custom status text.
    pub custom_status: Option<String>,
    /// Active rich activities.
    pub activities: Vec<Activity>,
}

/// Discord user profile summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Unique user Snowflake.
    pub id: Snowflake,
    /// Account handle (`username`).
    pub username: String,
    /// Optional global display name (`global_name`).
    pub global_name: Option<String>,
    /// Legacy 4-digit discriminator if non-zero (`"0"` omitted).
    pub discriminator: Option<String>,
    /// Avatar asset hash on Discord CDN.
    pub avatar_hash: Option<String>,
    /// Whether this account is a bot/application.
    pub bot: bool,
    /// Optional 24-bit RGB profile accent color.
    pub accent_color: Option<u32>,
    /// Last known presence status.
    pub presence: PresenceStatus,
}

impl User {
    /// Preferred human-readable display name (`global_name` falling back to `username`).
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.global_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.username)
    }

    /// Construct a CDN avatar URL when `avatar_hash` is set.
    #[must_use]
    pub fn avatar_url(&self, size: u16) -> Option<String> {
        let hash = self.avatar_hash.as_deref()?;
        let ext = if hash.starts_with("a_") { "gif" } else { "png" };
        Some(format!(
            "https://cdn.discordapp.com/avatars/{}/{hash}.{ext}?size={size}",
            self.id
        ))
    }
}

/// Discord server (guild) summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guild {
    /// Guild Snowflake.
    pub id: Snowflake,
    /// Guild display name.
    pub name: String,
    /// Optional CDN icon hash.
    pub icon_hash: Option<String>,
    /// Guild owner user ID when known.
    pub owner_id: Option<Snowflake>,
    /// Approximate member count when known.
    pub member_count: Option<u32>,
    /// Whether any channel in the guild has unread messages.
    pub unread: bool,
    /// Aggregated unread mention count across the guild.
    pub mention_count: u32,
}

/// Sidebar guild folder grouping matching Discord user settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuildFolder {
    /// Folder ID (`None` for a standalone ungrouped guild entry).
    pub id: Option<i64>,
    /// Optional user-defined folder label.
    pub name: Option<String>,
    /// Optional 24-bit RGB folder tint.
    pub color: Option<u32>,
    /// Ordered guild IDs inside this folder.
    pub guild_ids: Vec<Snowflake>,
    /// UI collapse state.
    pub collapsed: bool,
}

/// Discord channel kind covering guild text/voice/stage/threads/forums, 1:1 DMs, and Group DMs (`= 3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelKind {
    /// Standard guild text channel (Discord type `0`).
    GuildText,
    /// 1:1 Direct Message channel (Discord type `1`).
    DirectMessage,
    /// Guild voice channel (Discord type `2`).
    GuildVoice,
    /// Multi-user Group DM channel (Discord type `3`).
    GroupDm,
    /// Organizational category header (Discord type `4`).
    GuildCategory,
    /// Announcement / news channel (Discord type `5`).
    GuildAnnouncement,
    /// Public thread (Discord type `11`).
    PublicThread,
    /// Private thread (Discord type `12`).
    PrivateThread,
    /// Stage voice channel (Discord type `13`).
    GuildStageVoice,
    /// Forum channel (Discord type `15`).
    GuildForum,
    /// Forward-compatible unknown Discord channel type number.
    Other(u8),
}

impl ChannelKind {
    /// Convert a numeric Discord API `type` field into a [`ChannelKind`].
    #[must_use]
    pub const fn from_discord_type(raw: u8) -> Self {
        match raw {
            0 => Self::GuildText,
            1 => Self::DirectMessage,
            2 => Self::GuildVoice,
            3 => Self::GroupDm,
            4 => Self::GuildCategory,
            5 => Self::GuildAnnouncement,
            11 => Self::PublicThread,
            12 => Self::PrivateThread,
            13 => Self::GuildStageVoice,
            15 => Self::GuildForum,
            other => Self::Other(other),
        }
    }

    /// Return the numeric Discord API channel `type` code.
    #[must_use]
    pub const fn as_discord_type(self) -> u8 {
        match self {
            Self::GuildText => 0,
            Self::DirectMessage => 1,
            Self::GuildVoice => 2,
            Self::GroupDm => 3,
            Self::GuildCategory => 4,
            Self::GuildAnnouncement => 5,
            Self::PublicThread => 11,
            Self::PrivateThread => 12,
            Self::GuildStageVoice => 13,
            Self::GuildForum => 15,
            Self::Other(n) => n,
        }
    }

    /// Returns `true` for voice or stage voice channels.
    #[must_use]
    pub const fn is_voice(self) -> bool {
        matches!(self, Self::GuildVoice | Self::GuildStageVoice)
    }

    /// Returns `true` for 1:1 DMs or Group DMs.
    #[must_use]
    pub const fn is_dm(self) -> bool {
        matches!(self, Self::DirectMessage | Self::GroupDm)
    }

    /// Returns `true` specifically for Group DMs (`type == 3`).
    #[must_use]
    pub const fn is_group_dm(self) -> bool {
        matches!(self, Self::GroupDm)
    }

    /// Returns `true` for channels that can carry a live audio call (guild voice/stage or DM/Group DM).
    #[must_use]
    pub const fn supports_voice_call(self) -> bool {
        self.is_voice() || self.is_dm()
    }

    /// Returns `true` for thread channels.
    #[must_use]
    pub const fn is_thread(self) -> bool {
        matches!(self, Self::PublicThread | Self::PrivateThread)
    }

    /// Compact UTF-8 channel sigil for BBS channel trees.
    #[must_use]
    pub const fn utf8_sigil(self) -> &'static str {
        match self {
            Self::GuildText => "#",
            Self::DirectMessage => "@",
            Self::GroupDm => "&",
            Self::GuildVoice => "♪",
            Self::GuildStageVoice => "◈",
            Self::GuildCategory => "▸",
            Self::GuildAnnouncement => "★",
            Self::PublicThread | Self::PrivateThread => "↳",
            Self::GuildForum => "▤",
            Self::Other(_) => "?",
        }
    }

    /// Strict 7-bit ASCII channel sigil safe for HD44780 LCDs and serial terminals.
    #[must_use]
    pub const fn ascii_sigil(self) -> &'static str {
        match self {
            Self::GuildText => "#",
            Self::DirectMessage => "@",
            Self::GroupDm => "&",
            Self::GuildVoice => "V",
            Self::GuildStageVoice => "S",
            Self::GuildCategory => ">",
            Self::GuildAnnouncement => "!",
            Self::PublicThread | Self::PrivateThread => "+",
            Self::GuildForum => "F",
            Self::Other(_) => "?",
        }
    }
}

/// Guild channel, thread, 1:1 DM, or Group DM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// Channel Snowflake.
    pub id: Snowflake,
    /// Owning guild Snowflake (`None` for 1:1 DMs and Group DMs).
    pub guild_id: Option<Snowflake>,
    /// Discord channel type.
    pub kind: ChannelKind,
    /// Channel name (may be empty for unnamed 1:1 or Group DMs).
    pub name: String,
    /// Optional channel topic / header notice.
    pub topic: Option<String>,
    /// Parent category or parent text channel (for threads).
    pub parent_id: Option<Snowflake>,
    /// Ordering position within its guild/category.
    pub position: i32,
    /// Recipients for `DirectMessage` and `GroupDm` channels.
    pub recipients: Vec<User>,
    /// Owner ID for Group DMs and threads.
    pub owner_id: Option<Snowflake>,
    /// Optional Group DM custom icon hash.
    pub icon_hash: Option<String>,
    /// Most recent message Snowflake seen in this channel.
    pub last_message_id: Option<Snowflake>,
    /// Unread message counter.
    pub unread_count: u32,
    /// Unread direct mention counter.
    pub mention_count: u32,
    /// Age-restricted / NSFW flag.
    pub nsfw: bool,
    /// Per-user slowmode rate limit in seconds.
    pub slowmode_secs: u32,
}

impl Channel {
    /// Compute the user-facing label, synthesizing participant names for unnamed DMs / Group DMs.
    #[must_use]
    pub fn display_label(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.clone();
        }
        if !self.recipients.is_empty() {
            let names: Vec<&str> = self.recipients.iter().map(User::display_name).collect();
            return names.join(", ");
        }
        format!("channel-{}", self.id)
    }
}

/// Metadata specific to Discord voice messages (`IS_VOICE_MESSAGE` flag `1 << 13`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceMessageMeta {
    /// Total duration of the Ogg/Opus voice message in seconds.
    pub duration_secs: f32,
    /// Normalized waveform amplitude samples (`0..=255`) decoded from Discord's base64 waveform.
    pub waveform: Vec<u8>,
}

/// Refreshed signed Discord CDN attachment URL returned by `POST /attachments/refresh-urls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshedAttachmentUrl {
    /// Original (possibly expired) attachment URL.
    pub original_url: String,
    /// Fresh signed CDN URL.
    pub refreshed_url: String,
    /// Optional Unix timestamp (seconds) when the refreshed URL expires (`ex=` parameter).
    pub expires_at_unix: Option<i64>,
}

/// File, image, GIF, video, audio, or voice-message attachment on a [`Message`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment Snowflake.
    pub id: Snowflake,
    /// Original filename (e.g. `"screenshot.png"`, `"voice-message.ogg"`).
    pub filename: String,
    /// Optional MIME type (e.g. `"image/gif"`, `"audio/ogg"`).
    pub content_type: Option<String>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Primary signed CDN URL.
    pub url: String,
    /// Optional media proxy URL.
    pub proxy_url: Option<String>,
    /// Pixel width for image/video attachments.
    pub width: Option<u32>,
    /// Pixel height for image/video attachments.
    pub height: Option<u32>,
    /// Whether the attachment is ephemeral.
    pub ephemeral: bool,
    /// Optional CDN signed URL expiry in Unix seconds.
    pub expires_at_unix: Option<i64>,
    /// Voice-message waveform and duration metadata when present.
    pub voice_message: Option<VoiceMessageMeta>,
}

impl Attachment {
    /// Returns `true` if this attachment is a raster image (including animated GIF/WebP).
    #[must_use]
    pub fn is_image(&self) -> bool {
        if let Some(ct) = self.content_type.as_deref()
            && ct.starts_with("image/")
        {
            return true;
        }
        let lower = self.filename.to_ascii_lowercase();
        lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".webp")
            || lower.ends_with(".gif")
    }

    /// Returns `true` if this attachment is an animated GIF.
    #[must_use]
    pub fn is_gif(&self) -> bool {
        self.content_type.as_deref() == Some("image/gif")
            || self.filename.to_ascii_lowercase().ends_with(".gif")
    }

    /// Returns `true` if this attachment is a video file.
    #[must_use]
    pub fn is_video(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("video/"))
    }

    /// Returns `true` if this attachment is an audio file or Discord voice message.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        self.voice_message.is_some()
            || self
                .content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("audio/"))
    }

    /// Returns `true` if this attachment carries Discord voice-message waveform metadata.
    #[must_use]
    pub fn is_voice_message(&self) -> bool {
        self.voice_message.is_some()
    }
}

/// Media payload inside an [`Embed`] (hero image, link-card thumbnail, or video).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedMedia {
    /// Source media URL.
    pub url: String,
    /// Discord media proxy URL.
    pub proxy_url: Option<String>,
    /// Optional pixel width.
    pub width: Option<u32>,
    /// Optional pixel height.
    pub height: Option<u32>,
}

/// Provider metadata on an [`Embed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedProvider {
    /// Provider display name (e.g. `"GitHub"`, `"Tenor"`).
    pub name: Option<String>,
    /// Provider homepage URL.
    pub url: Option<String>,
}

/// Author header block on an [`Embed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedAuthor {
    /// Author name.
    pub name: String,
    /// Optional author URL.
    pub url: Option<String>,
    /// Optional author avatar/icon URL.
    pub icon_url: Option<String>,
    /// Optional proxied icon URL.
    pub proxy_icon_url: Option<String>,
}

/// Footer block on an [`Embed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedFooter {
    /// Footer text.
    pub text: String,
    /// Optional footer icon URL.
    pub icon_url: Option<String>,
    /// Optional proxied footer icon URL.
    pub proxy_icon_url: Option<String>,
}

/// Structured field inside an [`Embed`] supporting multi-column `inline` grid rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedField {
    /// Field title/header.
    pub name: String,
    /// Field body text (Markdown allowed).
    pub value: String,
    /// When `true`, consecutive inline fields flow horizontally into up to `max_columns` columns.
    pub inline: bool,
}

/// Discord embed kind discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum EmbedKind {
    /// Standard rich embed card.
    #[default]
    Rich,
    /// Bare image embed.
    Image,
    /// Video embed.
    Video,
    /// Animated GIFV embed (Tenor/Giphy/etc.).
    Gifv,
    /// Article link preview card.
    Article,
    /// Generic link preview card.
    Link,
    /// Poll result summary embed.
    PollResult,
    /// Forward-compatible unknown embed type.
    Unknown,
}

/// Rich Discord embed card supporting multi-column inline field grids and link-card thumbnails.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Embed {
    /// Embed classification.
    pub kind: EmbedKind,
    /// Optional title.
    pub title: Option<String>,
    /// Optional canonical link URL.
    pub url: Option<String>,
    /// Optional Markdown body description.
    pub description: Option<String>,
    /// Optional 24-bit RGB sidebar accent color (`0xRRGGBB`).
    pub color: Option<u32>,
    /// Optional ISO-8601 timestamp string.
    pub timestamp: Option<String>,
    /// Optional provider metadata.
    pub provider: Option<EmbedProvider>,
    /// Optional author header.
    pub author: Option<EmbedAuthor>,
    /// Optional footer block.
    pub footer: Option<EmbedFooter>,
    /// Optional full-width hero image.
    pub image: Option<EmbedMedia>,
    /// Optional side link-card thumbnail (preserved on rich, article, and link cards).
    pub thumbnail: Option<EmbedMedia>,
    /// Optional embedded video/GIFV stream.
    pub video: Option<EmbedMedia>,
    /// Ordered list of embed fields (`inline` and full-width).
    pub fields: Vec<EmbedField>,
}

impl Embed {
    /// Group [`Embed::fields`] into visual rows respecting Discord's `inline` grid semantics.
    ///
    /// - Consecutive `inline == true` fields are packed into rows of at most `max_columns.max(1)`.
    /// - Any `inline == false` field flushes the current inline row and occupies its own 1-element row.
    #[must_use]
    pub fn inline_field_rows(&self, max_columns: usize) -> Vec<Vec<&EmbedField>> {
        let cols = max_columns.max(1);
        let mut rows: Vec<Vec<&EmbedField>> = Vec::new();
        let mut current_inline: Vec<&EmbedField> = Vec::new();

        for field in &self.fields {
            if field.inline {
                current_inline.push(field);
                if current_inline.len() == cols {
                    rows.push(std::mem::take(&mut current_inline));
                }
            } else {
                if !current_inline.is_empty() {
                    rows.push(std::mem::take(&mut current_inline));
                }
                rows.push(vec![field]);
            }
        }
        if !current_inline.is_empty() {
            rows.push(current_inline);
        }
        rows
    }
}

/// Emoji reaction chip on a [`Message`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    /// Custom emoji Snowflake (`None` for standard Unicode emoji).
    pub emoji_id: Option<Snowflake>,
    /// Unicode glyph or custom emoji name.
    pub emoji_name: String,
    /// Whether this is an animated custom emoji.
    pub animated: bool,
    /// Total reaction count.
    pub count: u32,
    /// Whether the current user has reacted with this emoji.
    pub me: bool,
}

/// Single answer row inside a [`PollSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollAnswerSummary {
    /// Discord poll answer ID.
    pub id: u32,
    /// Answer label text.
    pub text: String,
    /// Optional leading emoji label.
    pub emoji: Option<String>,
    /// Vote tally for this option.
    pub votes: u32,
    /// Whether the current user voted for this option.
    pub me_voted: bool,
}

/// Compact summary of a Discord poll attached to a [`Message`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollSummary {
    /// Poll question text.
    pub question: String,
    /// Ordered poll answer summaries.
    pub answers: Vec<PollAnswerSummary>,
    /// Total votes across all options.
    pub total_votes: u32,
    /// Whether multiple options may be selected simultaneously.
    pub allow_multiselect: bool,
    /// Whether voting has closed.
    pub expired: bool,
    /// Optional ISO-8601 expiry timestamp.
    pub expires_at: Option<String>,
}

/// Reply target pointer attached to a [`Message`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageReference {
    /// Referenced message Snowflake.
    pub message_id: Option<Snowflake>,
    /// Referenced channel Snowflake.
    pub channel_id: Option<Snowflake>,
    /// Referenced guild Snowflake.
    pub guild_id: Option<Snowflake>,
    /// Cached display name of the replied-to author.
    pub author_name: Option<String>,
    /// Cached one-line preview snippet of the replied-to message.
    pub snippet: Option<String>,
}

/// Discord chat message including embeds, attachments, reactions, polls, and voice-message metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Message Snowflake.
    pub id: Snowflake,
    /// Channel Snowflake where this message lives.
    pub channel_id: Snowflake,
    /// Owning guild Snowflake (`None` for DMs and Group DMs).
    pub guild_id: Option<Snowflake>,
    /// Message author.
    pub author: User,
    /// Raw Discord Markdown message content.
    pub content: String,
    /// ISO-8601 creation timestamp.
    pub timestamp: String,
    /// Optional ISO-8601 last-edited timestamp.
    pub edited_timestamp: Option<String>,
    /// Whether the message is pinned.
    pub pinned: bool,
    /// Whether `@everyone`/`@here` was triggered.
    pub mention_everyone: bool,
    /// User Snowflakes explicitly mentioned in this message.
    pub mentions: Vec<Snowflake>,
    /// Optional reply target metadata.
    pub reply_to: Option<MessageReference>,
    /// File, image, GIF, video, and voice-message attachments.
    pub attachments: Vec<Attachment>,
    /// Rich embeds, link preview cards, and GIFV cards.
    pub embeds: Vec<Embed>,
    /// Emoji reactions.
    pub reactions: Vec<Reaction>,
    /// Optional Discord poll summary.
    pub poll: Option<PollSummary>,
    /// Discord message bitflags (e.g. `1 << 13` for `IS_VOICE_MESSAGE`).
    pub flags: u64,
    /// Optional client-generated nonce for optimistic send reconciliation.
    pub nonce: Option<String>,
}

impl Message {
    /// Discord `SUPPRESS_EMBEDS` message flag (`1 << 2`).
    pub const FLAG_SUPPRESS_EMBEDS: u64 = 1 << 2;
    /// Discord `IS_VOICE_MESSAGE` message flag (`1 << 13`).
    pub const FLAG_IS_VOICE_MESSAGE: u64 = 1 << 13;

    /// Returns `true` if this message is a Discord voice message.
    #[must_use]
    pub fn is_voice_message(&self) -> bool {
        (self.flags & Self::FLAG_IS_VOICE_MESSAGE) != 0
            || self.attachments.iter().any(Attachment::is_voice_message)
    }
}

/// Decoded RGBA8 animation or video frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbaFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Inter-frame display delay in milliseconds.
    pub delay_ms: u16,
    /// Tightly packed RGBA8 pixel buffer (`width * height * 4` bytes).
    pub pixels: Vec<u8>,
}

/// Upstream GIF catalog backend identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum GifProviderKind {
    /// Discord's first-party GIF search proxy (`/gifs/search`, `/gifs/trending`).
    DiscordProxy,
    /// Tenor API.
    Tenor,
    /// Giphy API.
    Giphy,
    /// Klipy API.
    Klipy,
    /// Built-in offline deterministic mock catalog.
    #[default]
    Mock,
}

/// Single searchable/sendable GIF item returned by a [`crate::traits::GifProvider`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifItem {
    /// Provider-scoped unique GIF identifier.
    pub id: String,
    /// Provider that supplied this item.
    pub provider: GifProviderKind,
    /// Human-readable title/description.
    pub title: String,
    /// Lightweight animated/static preview URL for the picker grid.
    pub preview_url: String,
    /// Canonical shareable GIF URL sent into Discord chat.
    pub gif_url: String,
    /// Intrinsic pixel width.
    pub width: u32,
    /// Intrinsic pixel height.
    pub height: u32,
    /// Optional frame count when known.
    pub frame_count: Option<u32>,
    /// Optional total loop duration in milliseconds.
    pub duration_ms: Option<u32>,
    /// Optional pre-decoded RGBA8 preview frames (used by offline mocks and decoded caches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_rgba_frames: Option<Vec<RgbaFrame>>,
}

/// Query parameters for searching or browsing GIFs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GifSearchQuery {
    /// Search terms (ignored when `trending == true` and empty).
    pub query: String,
    /// Maximum items requested per page.
    pub limit: u8,
    /// Optional pagination cursor returned by the previous page.
    pub cursor: Option<String>,
    /// Fetch trending GIFs instead of searching by keyword.
    pub trending: bool,
    /// Optional BCP-47 locale tag (e.g. `"en-US"`).
    pub locale: Option<String>,
}

impl Default for GifSearchQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 12,
            cursor: None,
            trending: true,
            locale: Some("en-US".to_string()),
        }
    }
}

/// Paginated GIF query result.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GifSearchPage {
    /// Items in this page.
    pub items: Vec<GifItem>,
    /// Optional continuation cursor for loading more items.
    pub next_cursor: Option<String>,
}

/// Discord Audio & Video End-to-End Encryption (DAVE / MLS) session state.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DaveSessionState {
    /// DAVE E2EE is not negotiated on this voice session.
    #[default]
    Disabled,
    /// MLS key package / commit handshake is in flight.
    Handshaking,
    /// DAVE E2EE is actively encrypting media frames.
    Active {
        /// Current MLS epoch number.
        epoch: u64,
        /// Negotiated DAVE protocol version.
        protocol_version: u16,
        /// Voice privacy verification code shown to users for out-of-band comparison.
        verification_code: String,
    },
    /// Session fell back or encountered a DAVE verification issue.
    Degraded {
        /// Human-readable reason for degraded status.
        reason: String,
    },
}

/// Connection lifecycle for an active voice session (guild voice or DM/Group DM call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum VoiceConnectionState {
    /// Not connected to any voice channel or call.
    #[default]
    Disconnected,
    /// Opening voice WebSocket / awaiting voice server info.
    Connecting,
    /// Performing UDP IP discovery, ICE/WebRTC signaling, or DAVE handshake.
    Signaling,
    /// Actively exchanging Opus audio packets.
    Connected,
    /// Transient disconnect; attempting automatic resume.
    Reconnecting,
}

/// Per-user voice participant state inside an active guild voice channel or DM/Group DM call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceParticipant {
    /// Participant user Snowflake.
    pub user_id: Snowflake,
    /// Display name shown in the voice deck.
    pub display_name: String,
    /// Whether this user is currently transmitting speech.
    pub speaking: bool,
    /// User muted their own microphone.
    pub self_mute: bool,
    /// User deafened their own audio output.
    pub self_deaf: bool,
    /// Server-enforced mute.
    pub server_mute: bool,
    /// Server-enforced deafen.
    pub server_deaf: bool,
    /// Local playback gain percentage (`0..=200`, where `100` = unity 0 dB).
    pub volume: u8,
    /// Smoothed short-window RMS level in dBFS (`-60.0..=0.0`) for Braille VU meters.
    pub rms_db: f32,
    /// Whether this participant's DAVE E2EE identity has been verified in the current MLS epoch.
    pub dave_verified: bool,
    /// Whether this participant is currently broadcasting a Go Live screenshare.
    pub streaming: bool,
}

impl VoiceParticipant {
    /// Minimum RMS dBFS floor mapped to `0.0` on VU meters.
    pub const MIN_RMS_DB: f32 = -60.0;
    /// Maximum allowed per-user gain percentage (`200%`).
    pub const MAX_VOLUME: u8 = 200;

    /// Set local participant volume, clamping safely to `0..=200`.
    pub fn set_volume(&mut self, volume: u8) {
        self.volume = volume.min(Self::MAX_VOLUME);
    }

    /// Normalize [`Self::rms_db`] (`-60.0..=0.0` dBFS) into `[0.0, 1.0]` for Braille VU rendering.
    #[must_use]
    pub fn normalized_vu(&self) -> f32 {
        let clamped = self.rms_db.clamp(Self::MIN_RMS_DB, 0.0);
        (clamped - Self::MIN_RMS_DB) / -Self::MIN_RMS_DB
    }
}

/// Ringing state for 1:1 DM and Group DM voice calls.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CallRingState {
    /// No active or ringing DM call.
    #[default]
    Idle,
    /// Local user is ringing one or more DM/Group DM recipients.
    OutgoingRinging {
        /// Target DM or Group DM channel ID.
        channel_id: Snowflake,
        /// Recipient user IDs currently being rung.
        target_user_ids: Vec<Snowflake>,
    },
    /// Incoming call ring from another user in a 1:1 DM or Group DM.
    IncomingRinging {
        /// DM or Group DM channel ID.
        channel_id: Snowflake,
        /// Caller user ID.
        caller_id: Snowflake,
        /// Caller display name.
        caller_name: String,
        /// `true` when this call belongs to a multi-user Group DM.
        is_group_dm: bool,
    },
    /// Call has been answered and has active connected participants.
    Active {
        /// DM or Group DM channel ID.
        channel_id: Snowflake,
        /// Currently connected participant IDs.
        connected_user_ids: Vec<Snowflake>,
    },
}

/// Unified snapshot of an active voice session (guild voice, stage, 1:1 DM call, or Group DM call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceChannelStatus {
    /// Owning guild Snowflake (`None` for 1:1 DM and Group DM calls).
    pub guild_id: Option<Snowflake>,
    /// Voice channel or DM/Group DM channel Snowflake.
    pub channel_id: Snowflake,
    /// Human-readable voice channel or call name.
    pub channel_name: String,
    /// Current connection lifecycle state.
    pub connection_state: VoiceConnectionState,
    /// Local microphone mute state.
    pub self_mute: bool,
    /// Local speaker deafen state.
    pub self_deaf: bool,
    /// Push-to-talk mode enabled.
    pub push_to_talk: bool,
    /// Measured voice UDP/WebRTC round-trip latency in milliseconds.
    pub ping_ms: Option<u32>,
    /// Rolling packet loss percentage (`0.0..=100.0`).
    pub packet_loss_pct: f32,
    /// DAVE E2EE status for this session.
    pub dave: DaveSessionState,
    /// DM/Group DM call ring state (`CallRingState::Idle` for guild voice channels).
    pub call_ring: CallRingState,
    /// Participants currently in the voice channel or call.
    pub participants: Vec<VoiceParticipant>,
}

/// Playback progress for an inline Discord voice message attachment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoicePlaybackState {
    /// Owning message Snowflake.
    pub message_id: Snowflake,
    /// Playing attachment Snowflake.
    pub attachment_id: Snowflake,
    /// Whether audio is actively playing.
    pub playing: bool,
    /// Current playback head in seconds.
    pub position_secs: f32,
    /// Total duration in seconds.
    pub duration_secs: f32,
}

/// Output target for rendering a watched Go Live screenshare stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum VideoSinkKind {
    /// Hardware-accelerated native pop-out window (e.g. Wayland/X11 GStreamer sink).
    #[default]
    PopoutWindow,
    /// Inline terminal viewport rendering (Kitty/Sixel/iTerm2/HalfBlock).
    InlineTerminal,
}

/// Video capture source when broadcasting a local Go Live screenshare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum StreamSourceKind {
    /// Wayland XDG Desktop Portal + PipeWire screencast picker.
    #[default]
    PortalPipeWire,
    /// Specific application window capture.
    ApplicationWindow,
    /// Entire physical monitor capture.
    Monitor,
    /// Synthetic offline SMPTE test-card generator (for CI and `--mock-demo`).
    SyntheticTestCard,
}

/// Requested encoding parameters for outbound Go Live broadcasting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamQuality {
    /// Target frame width in pixels.
    pub width: u32,
    /// Target frame height in pixels.
    pub height: u32,
    /// Target frames per second.
    pub fps: u8,
    /// Maximum video bitrate in kilobits per second.
    pub max_bitrate_kbps: u32,
}

impl Default for StreamQuality {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            max_bitrate_kbps: 2_500,
        }
    }
}

/// Metadata describing an active Go Live screenshare stream in a voice channel or DM call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDescriptor {
    /// Canonical Discord Go Live stream key (e.g. `"guild:<g>:<c>:<u>"` or `"call:<c>:<u>"`).
    pub stream_key: String,
    /// Owning guild Snowflake (`None` for DM/Group DM calls).
    pub guild_id: Option<Snowflake>,
    /// Voice channel or DM/Group DM channel Snowflake.
    pub channel_id: Snowflake,
    /// Broadcaster user Snowflake.
    pub owner_id: Snowflake,
    /// Broadcaster display name.
    pub owner_name: String,
    /// Stream / window title shown in the stage deck.
    pub title: String,
    /// Stream resolution `(width, height)` in pixels.
    pub resolution: (u32, u32),
    /// Nominal framerate in FPS.
    pub fps: u8,
    /// Current number of watching viewers.
    pub viewer_count: u32,
    /// Whether stream audio is included.
    pub has_audio: bool,
    /// Whether this stream is protected by DAVE E2EE.
    pub dave_encrypted: bool,
}

/// Decoded RGBA8 video frame emitted by a [`crate::traits::VideoStreamEngine`] for inline sinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    /// Source stream key.
    pub stream_key: String,
    /// Monotonically increasing frame sequence number.
    pub seq: u64,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Tightly packed RGBA8 pixel data (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_serde_roundtrip_and_epoch() {
        let sf = Snowflake::from_str("175928847299117063").unwrap();
        assert_eq!(sf.timestamp_ms(), 1_462_015_105_796);
        let json = serde_json::to_string(&sf).unwrap();
        assert_eq!(json, "\"175928847299117063\"");
        let parsed_str: Snowflake = serde_json::from_str(&json).unwrap();
        let parsed_num: Snowflake = serde_json::from_str("175928847299117063").unwrap();
        assert_eq!(parsed_str, sf);
        assert_eq!(parsed_num, sf);
        assert!(Snowflake::from_str("not-a-number").is_err());
    }

    #[test]
    fn channel_kind_group_dm_is_three() {
        assert_eq!(ChannelKind::GroupDm.as_discord_type(), 3);
        assert_eq!(ChannelKind::from_discord_type(3), ChannelKind::GroupDm);
        assert!(ChannelKind::GroupDm.is_group_dm());
        assert!(ChannelKind::GroupDm.is_dm());
        assert!(ChannelKind::GroupDm.supports_voice_call());
        assert_eq!(ChannelKind::from_discord_type(13), ChannelKind::GuildStageVoice);
        assert_eq!(ChannelKind::from_discord_type(15), ChannelKind::GuildForum);
    }

    #[test]
    fn embed_inline_field_rows_pack_multi_column_grids() {
        let embed = Embed {
            fields: vec![
                EmbedField { name: "A".into(), value: "1".into(), inline: true },
                EmbedField { name: "B".into(), value: "2".into(), inline: true },
                EmbedField { name: "C".into(), value: "3".into(), inline: true },
                EmbedField { name: "D".into(), value: "4".into(), inline: true },
                EmbedField { name: "Full".into(), value: "5".into(), inline: false },
                EmbedField { name: "E".into(), value: "6".into(), inline: true },
            ],
            ..Default::default()
        };

        let wide_rows = embed.inline_field_rows(3);
        assert_eq!(wide_rows.len(), 4);
        assert_eq!(wide_rows[0].len(), 3); // A, B, C
        assert_eq!(wide_rows[1].len(), 1); // D (flushed before Full)
        assert_eq!(wide_rows[2].len(), 1); // Full
        assert_eq!(wide_rows[3].len(), 1); // E

        let std_rows = embed.inline_field_rows(2);
        assert_eq!(std_rows.len(), 4);
        assert_eq!(std_rows[0].len(), 2); // A, B
        assert_eq!(std_rows[1].len(), 2); // C, D
        assert_eq!(std_rows[2].len(), 1); // Full
        assert_eq!(std_rows[3].len(), 1); // E

        let compact_rows = embed.inline_field_rows(1);
        assert_eq!(compact_rows.len(), 6);
    }

    #[test]
    fn voice_participant_volume_clamping_and_vu_normalization() {
        let mut p = VoiceParticipant {
            user_id: Snowflake(1),
            display_name: "sysop".into(),
            speaking: true,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
            volume: 100,
            rms_db: -30.0,
            dave_verified: true,
            streaming: false,
        };
        assert!((p.normalized_vu() - 0.5).abs() < 0.001);
        p.rms_db = -90.0;
        assert!((p.normalized_vu() - 0.0).abs() < f32::EPSILON);
        p.rms_db = 3.0;
        assert!((p.normalized_vu() - 1.0).abs() < f32::EPSILON);
        p.set_volume(255);
        assert_eq!(p.volume, 200);
    }
}
