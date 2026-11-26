//! Defensive Discord API v10 and Gateway v10 wire DTOs and conversions to [`bbs_core`] models.
//!
//! Every deserializer tolerates missing optional fields, unknown variants, and Discord's
//! mixed string/integer encodings (such as `nonce` or `guild_folders[].id`) without panicking.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bbs_core::{
    Activity, ActivityKind, Attachment, Channel, ChannelKind, Embed, EmbedAuthor, EmbedField,
    EmbedFooter, EmbedKind, EmbedMedia, EmbedProvider, Guild, GuildFolder, Message,
    MessageReference, PollAnswerSummary, PollSummary, Presence, PresenceStatus, Reaction,
    RefreshedAttachmentUrl, Snowflake, User, VoiceMessageMeta,
};
use serde::{Deserialize, Deserializer, Serialize};
use url::Url;

/// Maximum characters kept for a reply preview snippet.
const MAX_REPLY_SNIPPET_CHARS: usize = 120;

/// Extract Discord's signed CDN `ex=<hex_unix_secs>` expiration query parameter, if present.
#[must_use]
pub fn extract_cdn_expiry_unix(raw_url: &str) -> Option<i64> {
    let parsed = Url::parse(raw_url).ok()?;
    for (key, value) in parsed.query_pairs() {
        if key == "ex" {
            return i64::from_str_radix(value.trim(), 16).ok();
        }
    }
    None
}

/// Deserialize a Discord value that may be encoded as a JSON string, integer, or `null` into `Option<String>`.
pub(crate) fn deserialize_opt_string_or_number<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match val {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::Bool(b)) => Some(b.to_string()),
        Some(_) => None,
    })
}

/// Deserialize a Discord value that may be encoded as an integer, decimal string, or `null` into `Option<i64>`.
pub(crate) fn deserialize_opt_i64_flexible<'de, D>(
    deserializer: D,
) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match val {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n.as_i64().or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok())),
        Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(_) => None,
    })
}

/// Deserialize a `Vec<T>` treating JSON `null` as empty.
pub(crate) fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Wire representation of a Discord User object.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireUser {
    /// User Snowflake.
    pub id: Snowflake,
    /// Account handle.
    #[serde(default)]
    pub username: String,
    /// Optional global display name.
    #[serde(default)]
    pub global_name: Option<String>,
    /// Legacy 4-digit discriminator (`"0"` for migrated accounts).
    #[serde(default)]
    pub discriminator: Option<String>,
    /// CDN avatar hash.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Bot account flag.
    #[serde(default)]
    pub bot: bool,
    /// Optional 24-bit RGB profile accent color.
    #[serde(default)]
    pub accent_color: Option<u32>,
}

impl From<WireUser> for User {
    fn from(wire: WireUser) -> Self {
        let discriminator = wire
            .discriminator
            .filter(|d| !d.is_empty() && d != "0" && d != "0000");
        Self {
            id: wire.id,
            username: wire.username,
            global_name: wire.global_name.filter(|s| !s.is_empty()),
            discriminator,
            avatar_hash: wire.avatar.filter(|s| !s.is_empty()),
            bot: wire.bot,
            accent_color: wire.accent_color,
            presence: PresenceStatus::Offline,
        }
    }
}

/// Nested guild properties sent in modern Gateway v10 `READY` payloads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireGuildProperties {
    /// Guild name.
    #[serde(default)]
    pub name: String,
    /// Guild icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    /// Guild owner Snowflake.
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
}

/// Wire representation of a Discord Guild (supporting both REST flat and Gateway `READY` nested shapes).
#[derive(Debug, Clone, Deserialize)]
pub struct WireGuild {
    /// Guild Snowflake.
    pub id: Snowflake,
    /// Top-level guild name (REST `/users/@me/guilds` or legacy `READY`).
    #[serde(default)]
    pub name: Option<String>,
    /// Top-level icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    /// Top-level owner ID.
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    /// Approximate or reported member count.
    #[serde(default)]
    pub member_count: Option<u32>,
    /// Approximate member count returned by REST with `with_counts=true`.
    #[serde(default)]
    pub approximate_member_count: Option<u32>,
    /// Nested properties object present in Gateway `READY` user payloads.
    #[serde(default)]
    pub properties: Option<WireGuildProperties>,
    /// Channels embedded in Gateway `READY` / `GUILD_CREATE`.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub channels: Vec<WireChannel>,
    /// Active voice states embedded in Gateway `READY` / `GUILD_CREATE`.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub voice_states: Vec<WireVoiceState>,
}

impl WireGuild {
    /// Convert into the core [`Guild`] summary model.
    #[must_use]
    pub fn to_core_guild(&self) -> Guild {
        let props = self.properties.as_ref();
        let name = self
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| props.map(|p| p.name.as_str()).filter(|s| !s.is_empty()))
            .unwrap_or("Unknown Guild")
            .to_owned();
        let icon_hash = self
            .icon
            .clone()
            .or_else(|| props.and_then(|p| p.icon.clone()))
            .filter(|s| !s.is_empty());
        let owner_id = self.owner_id.or_else(|| props.and_then(|p| p.owner_id));
        let member_count = self.member_count.or(self.approximate_member_count);

        Guild {
            id: self.id,
            name,
            icon_hash,
            owner_id,
            member_count,
            unread: false,
            mention_count: 0,
            roles: Vec::new(),
            muted: false,
            suppress_everyone: false,
            suppress_roles: false,
        }
    }
}

/// Wire representation of a sidebar guild folder from `user_settings.guild_folders`.
#[derive(Debug, Clone, Deserialize)]
pub struct WireGuildFolder {
    /// Folder ID (`null` for standalone guild entries).
    #[serde(default, deserialize_with = "deserialize_opt_i64_flexible")]
    pub id: Option<i64>,
    /// Optional custom label.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional 24-bit RGB folder color.
    #[serde(default)]
    pub color: Option<u32>,
    /// Ordered guild Snowflakes.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub guild_ids: Vec<Snowflake>,
}

impl From<WireGuildFolder> for GuildFolder {
    fn from(wire: WireGuildFolder) -> Self {
        Self {
            id: wire.id,
            name: wire.name.filter(|s| !s.is_empty()),
            color: wire.color,
            guild_ids: wire.guild_ids,
            collapsed: false,
        }
    }
}

/// Wire representation of a Discord Channel (guild text/voice/thread/forum, 1:1 DM, Group DM).
#[derive(Debug, Clone, Deserialize)]
pub struct WireChannel {
    /// Channel Snowflake.
    pub id: Snowflake,
    /// Owning guild Snowflake (`None` for 1:1 DMs and Group DMs).
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// Numeric Discord channel type (`3` = Group DM).
    #[serde(rename = "type", default)]
    pub kind: u8,
    /// Optional channel name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional channel topic.
    #[serde(default)]
    pub topic: Option<String>,
    /// Parent category or parent channel ID.
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
    /// Display ordering position.
    #[serde(default)]
    pub position: Option<i32>,
    /// Inline recipients (REST `/users/@me/channels` and non-deduped events).
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub recipients: Vec<WireUser>,
    /// Deduplicated recipient user Snowflakes (Gateway `READY` with `DEDUPE_USER_OBJECTS`).
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub recipient_ids: Vec<Snowflake>,
    /// Group DM or thread owner ID.
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    /// Custom Group DM icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    /// Most recent message Snowflake.
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    /// NSFW flag.
    #[serde(default)]
    pub nsfw: bool,
    /// Slowmode delay in seconds.
    #[serde(default)]
    pub rate_limit_per_user: u32,
}

impl WireChannel {
    /// Convert into [`Channel`], optionally resolving `recipient_ids` via `users_by_id` and
    /// injecting `fallback_guild_id` when Discord omits `guild_id` inside `READY.guilds[].channels`.
    #[must_use]
    pub fn into_core_channel(
        self,
        fallback_guild_id: Option<Snowflake>,
        users_by_id: &HashMap<Snowflake, User>,
    ) -> Channel {
        let kind = ChannelKind::from_discord_type(self.kind);
        let guild_id = if kind.is_dm() {
            None
        } else {
            self.guild_id.or(fallback_guild_id)
        };

        let recipients = if !self.recipients.is_empty() {
            self.recipients.into_iter().map(User::from).collect()
        } else {
            self.recipient_ids
                .into_iter()
                .map(|uid| {
                    users_by_id.get(&uid).cloned().unwrap_or_else(|| User {
                        id: uid,
                        username: format!("user-{uid}"),
                        global_name: None,
                        discriminator: None,
                        avatar_hash: None,
                        bot: false,
                        accent_color: None,
                        presence: PresenceStatus::Offline,
                    })
                })
                .collect()
        };

        Channel {
            id: self.id,
            guild_id,
            kind,
            name: self.name.unwrap_or_default(),
            topic: self.topic.filter(|s| !s.is_empty()),
            parent_id: self.parent_id,
            position: self.position.unwrap_or(0),
            recipients,
            owner_id: self.owner_id,
            icon_hash: self.icon.filter(|s| !s.is_empty()),
            last_message_id: self.last_message_id,
            unread_count: 0,
            mention_count: 0,
            nsfw: self.nsfw,
            slowmode_secs: self.rate_limit_per_user,
            muted: false,
            last_ack_message_id: None,
        }
    }
}

/// Wire representation of a Discord Attachment object.
#[derive(Debug, Clone, Deserialize)]
pub struct WireAttachment {
    /// Attachment Snowflake.
    pub id: Snowflake,
    /// Original filename.
    #[serde(default)]
    pub filename: String,
    /// Optional MIME type.
    #[serde(default)]
    pub content_type: Option<String>,
    /// File size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Signed CDN URL.
    #[serde(default)]
    pub url: String,
    /// Proxied CDN URL.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Pixel width.
    #[serde(default)]
    pub width: Option<u32>,
    /// Pixel height.
    #[serde(default)]
    pub height: Option<u32>,
    /// Ephemeral flag.
    #[serde(default)]
    pub ephemeral: bool,
    /// Voice message duration in seconds.
    #[serde(default)]
    pub duration_secs: Option<f32>,
    /// Base64-encoded voice message waveform bytes (`0..=255`).
    #[serde(default)]
    pub waveform: Option<String>,
}

impl From<WireAttachment> for Attachment {
    fn from(wire: WireAttachment) -> Self {
        let expires_at_unix = extract_cdn_expiry_unix(&wire.url);
        let voice_message = match (wire.duration_secs, wire.waveform) {
            (Some(duration_secs), Some(waveform_b64)) => {
                let waveform = BASE64_STANDARD.decode(waveform_b64.trim()).unwrap_or_default();
                Some(VoiceMessageMeta {
                    duration_secs,
                    waveform,
                })
            }
            (Some(duration_secs), None) => Some(VoiceMessageMeta {
                duration_secs,
                waveform: Vec::new(),
            }),
            _ => None,
        };

        Self {
            id: wire.id,
            filename: wire.filename,
            content_type: wire.content_type,
            size_bytes: wire.size,
            url: wire.url,
            proxy_url: wire.proxy_url,
            width: wire.width,
            height: wire.height,
            ephemeral: wire.ephemeral,
            expires_at_unix,
            voice_message,
        }
    }
}

/// Wire embed media block.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmbedMedia {
    /// Source URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Discord proxy URL.
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// Width in pixels.
    #[serde(default)]
    pub width: Option<u32>,
    /// Height in pixels.
    #[serde(default)]
    pub height: Option<u32>,
}

impl WireEmbedMedia {
    fn into_core(self) -> Option<EmbedMedia> {
        let url = self.url.or_else(|| self.proxy_url.clone())?;
        if url.is_empty() {
            return None;
        }
        Some(EmbedMedia {
            url,
            proxy_url: self.proxy_url,
            width: self.width,
            height: self.height,
        })
    }
}

/// Wire embed provider.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmbedProvider {
    /// Provider name.
    #[serde(default)]
    pub name: Option<String>,
    /// Provider URL.
    #[serde(default)]
    pub url: Option<String>,
}

/// Wire embed author.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmbedAuthor {
    /// Author name.
    #[serde(default)]
    pub name: String,
    /// Optional link URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Optional icon URL.
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Optional proxied icon URL.
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

/// Wire embed footer.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmbedFooter {
    /// Footer text.
    #[serde(default)]
    pub text: String,
    /// Optional icon URL.
    #[serde(default)]
    pub icon_url: Option<String>,
    /// Optional proxied icon URL.
    #[serde(default)]
    pub proxy_icon_url: Option<String>,
}

/// Wire embed field.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmbedField {
    /// Field header.
    #[serde(default)]
    pub name: String,
    /// Field body.
    #[serde(default)]
    pub value: String,
    /// Inline grid layout flag.
    #[serde(default)]
    pub inline: bool,
}

/// Wire representation of a Discord Embed.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmbed {
    /// Embed type string (`"rich"`, `"image"`, `"video"`, `"gifv"`, `"article"`, `"link"`, `"poll_result"`).
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// Title text.
    #[serde(default)]
    pub title: Option<String>,
    /// Canonical URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Description body.
    #[serde(default)]
    pub description: Option<String>,
    /// 24-bit RGB accent color.
    #[serde(default)]
    pub color: Option<u32>,
    /// ISO-8601 timestamp.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Provider metadata.
    #[serde(default)]
    pub provider: Option<WireEmbedProvider>,
    /// Author block.
    #[serde(default)]
    pub author: Option<WireEmbedAuthor>,
    /// Footer block.
    #[serde(default)]
    pub footer: Option<WireEmbedFooter>,
    /// Hero image.
    #[serde(default)]
    pub image: Option<WireEmbedMedia>,
    /// Link-card thumbnail.
    #[serde(default)]
    pub thumbnail: Option<WireEmbedMedia>,
    /// Video / GIFV stream.
    #[serde(default)]
    pub video: Option<WireEmbedMedia>,
    /// Structured fields.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub fields: Vec<WireEmbedField>,
}

impl From<WireEmbed> for Embed {
    fn from(wire: WireEmbed) -> Self {
        let kind = match wire.kind.as_deref() {
            Some("rich") | None => EmbedKind::Rich,
            Some("image") => EmbedKind::Image,
            Some("video") => EmbedKind::Video,
            Some("gifv") => EmbedKind::Gifv,
            Some("article") => EmbedKind::Article,
            Some("link") => EmbedKind::Link,
            Some("poll_result") => EmbedKind::PollResult,
            Some(_) => EmbedKind::Unknown,
        };
        Self {
            kind,
            title: wire.title,
            url: wire.url,
            description: wire.description,
            color: wire.color,
            timestamp: wire.timestamp,
            provider: wire.provider.map(|p| EmbedProvider {
                name: p.name,
                url: p.url,
            }),
            author: wire.author.map(|a| EmbedAuthor {
                name: a.name,
                url: a.url,
                icon_url: a.icon_url,
                proxy_icon_url: a.proxy_icon_url,
            }),
            footer: wire.footer.map(|f| EmbedFooter {
                text: f.text,
                icon_url: f.icon_url,
                proxy_icon_url: f.proxy_icon_url,
            }),
            image: wire.image.and_then(WireEmbedMedia::into_core),
            thumbnail: wire.thumbnail.and_then(WireEmbedMedia::into_core),
            video: wire.video.and_then(WireEmbedMedia::into_core),
            fields: wire
                .fields
                .into_iter()
                .map(|f| EmbedField {
                    name: f.name,
                    value: f.value,
                    inline: f.inline,
                })
                .collect(),
        }
    }
}

/// Wire emoji identifier inside reactions or polls.
#[derive(Debug, Clone, Deserialize)]
pub struct WireEmoji {
    /// Custom emoji Snowflake (`None` for Unicode emojis).
    #[serde(default)]
    pub id: Option<Snowflake>,
    /// Unicode glyph or custom emoji name.
    #[serde(default)]
    pub name: Option<String>,
    /// Animated flag.
    #[serde(default)]
    pub animated: bool,
}

/// Wire message reaction object.
#[derive(Debug, Clone, Deserialize)]
pub struct WireReaction {
    /// Reaction emoji.
    pub emoji: WireEmoji,
    /// Total reaction count.
    #[serde(default)]
    pub count: u32,
    /// Whether the current user reacted.
    #[serde(default)]
    pub me: bool,
}

impl From<WireReaction> for Reaction {
    fn from(wire: WireReaction) -> Self {
        Self {
            emoji_id: wire.emoji.id,
            emoji_name: wire.emoji.name.unwrap_or_default(),
            animated: wire.emoji.animated,
            count: wire.count,
            me: wire.me,
        }
    }
}

/// Wire poll media object.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WirePollMedia {
    /// Text content.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional emoji.
    #[serde(default)]
    pub emoji: Option<WireEmoji>,
}

/// Wire poll answer option.
#[derive(Debug, Clone, Deserialize)]
pub struct WirePollAnswer {
    /// Answer ID.
    pub answer_id: u32,
    /// Answer label and emoji.
    #[serde(default)]
    pub poll_media: WirePollMedia,
}

/// Wire poll answer vote count.
#[derive(Debug, Clone, Deserialize)]
pub struct WirePollAnswerCount {
    /// Answer ID.
    pub id: u32,
    /// Vote count.
    #[serde(default)]
    pub count: u32,
    /// Whether the signed-in user voted for this answer.
    #[serde(default)]
    pub me_voted: bool,
}

/// Wire poll tally block.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WirePollResults {
    /// Whether voting has closed and tallies are final.
    #[serde(default)]
    pub is_finalized: bool,
    /// Per-answer vote counts.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub answer_counts: Vec<WirePollAnswerCount>,
}

/// Wire Discord poll object.
#[derive(Debug, Clone, Deserialize)]
pub struct WirePoll {
    /// Question media.
    #[serde(default)]
    pub question: WirePollMedia,
    /// Answer options.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub answers: Vec<WirePollAnswer>,
    /// Optional ISO-8601 expiry timestamp.
    #[serde(default)]
    pub expiry: Option<String>,
    /// Whether multiple choices are allowed.
    #[serde(default)]
    pub allow_multiselect: bool,
    /// Current tally results.
    #[serde(default)]
    pub results: Option<WirePollResults>,
}

impl From<WirePoll> for PollSummary {
    fn from(wire: WirePoll) -> Self {
        let counts_by_id: HashMap<u32, (u32, bool)> = wire
            .results
            .as_ref()
            .map(|r| {
                r.answer_counts
                    .iter()
                    .map(|c| (c.id, (c.count, c.me_voted)))
                    .collect()
            })
            .unwrap_or_default();

        let mut total_votes = 0u32;
        let answers = wire
            .answers
            .into_iter()
            .map(|a| {
                let (votes, me_voted) = counts_by_id.get(&a.answer_id).copied().unwrap_or((0, false));
                total_votes = total_votes.saturating_add(votes);
                PollAnswerSummary {
                    id: a.answer_id,
                    text: a.poll_media.text.unwrap_or_default(),
                    emoji: a.poll_media.emoji.and_then(|e| e.name),
                    votes,
                    me_voted,
                }
            })
            .collect();

        Self {
            question: wire.question.text.unwrap_or_default(),
            answers,
            total_votes,
            allow_multiselect: wire.allow_multiselect,
            expired: wire.results.as_ref().is_some_and(|r| r.is_finalized),
            expires_at: wire.expiry,
        }
    }
}

/// Wire message reference object (reply target).
#[derive(Debug, Clone, Deserialize)]
pub struct WireMessageReference {
    /// Referenced message Snowflake.
    #[serde(default)]
    pub message_id: Option<Snowflake>,
    /// Referenced channel Snowflake.
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    /// Referenced guild Snowflake.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
}

/// Lightweight wire summary of a `referenced_message` included in reply payloads.
#[derive(Debug, Clone, Deserialize)]
pub struct WireReferencedMessage {
    /// Author of the referenced message.
    #[serde(default)]
    pub author: Option<WireUser>,
    /// Content of the referenced message.
    #[serde(default)]
    pub content: String,
}

/// Wire representation of a Discord Message.
#[derive(Debug, Clone, Deserialize)]
pub struct WireMessage {
    /// Message Snowflake.
    pub id: Snowflake,
    /// Channel Snowflake.
    pub channel_id: Snowflake,
    /// Optional guild Snowflake.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// Author (may be omitted in partial `MESSAGE_UPDATE` embed-unfurl dispatches).
    #[serde(default)]
    pub author: Option<WireUser>,
    /// Raw Markdown content.
    #[serde(default)]
    pub content: String,
    /// ISO-8601 timestamp.
    #[serde(default)]
    pub timestamp: String,
    /// Optional ISO-8601 edit timestamp.
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    /// Pin flag.
    #[serde(default)]
    pub pinned: bool,
    /// Everyone/here mention flag.
    #[serde(default)]
    pub mention_everyone: bool,
    /// Mentioned user objects.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub mentions: Vec<WireUser>,
    /// Optional reply reference.
    #[serde(default)]
    pub message_reference: Option<WireMessageReference>,
    /// Optional inline referenced message payload.
    #[serde(default)]
    pub referenced_message: Option<Box<WireReferencedMessage>>,
    /// Attachments.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub attachments: Vec<WireAttachment>,
    /// Embeds.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub embeds: Vec<WireEmbed>,
    /// Reactions.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub reactions: Vec<WireReaction>,
    /// Optional poll.
    #[serde(default)]
    pub poll: Option<WirePoll>,
    /// Message bitflags.
    #[serde(default)]
    pub flags: u64,
    /// Optional client nonce (string or integer).
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    pub nonce: Option<String>,
}

impl From<WireMessage> for Message {
    fn from(wire: WireMessage) -> Self {
        let author = wire.author.map(User::from).unwrap_or_else(|| User {
            id: Snowflake(0),
            username: "unknown".to_owned(),
            global_name: None,
            discriminator: None,
            avatar_hash: None,
            bot: false,
            accent_color: None,
            presence: PresenceStatus::Offline,
        });

        let reply_to = wire.message_reference.map(|r| {
            let (author_name, snippet) = match wire.referenced_message.as_deref() {
                Some(ref_msg) => {
                    let name = ref_msg
                        .author
                        .as_ref()
                        .map(|a| {
                            a.global_name
                                .as_deref()
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&a.username)
                                .to_owned()
                        });
                    let trimmed = ref_msg.content.trim();
                    let snip = if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.chars().take(MAX_REPLY_SNIPPET_CHARS).collect())
                    };
                    (name, snip)
                }
                None => (None, None),
            };
            MessageReference {
                message_id: r.message_id,
                channel_id: r.channel_id,
                guild_id: r.guild_id,
                author_name,
                snippet,
            }
        });

        Self {
            id: wire.id,
            channel_id: wire.channel_id,
            guild_id: wire.guild_id,
            author,
            content: wire.content,
            timestamp: wire.timestamp,
            edited_timestamp: wire.edited_timestamp,
            pinned: wire.pinned,
            mention_everyone: wire.mention_everyone,
            mentions: wire.mentions.into_iter().map(|u| u.id).collect(),
            reply_to,
            attachments: wire.attachments.into_iter().map(Attachment::from).collect(),
            embeds: wire.embeds.into_iter().map(Embed::from).collect(),
            reactions: wire.reactions.into_iter().map(Reaction::from).collect(),
            poll: wire.poll.map(PollSummary::from),
            flags: wire.flags,
            nonce: wire.nonce,
            author_nick: None,
            author_role_ids: Vec::new(),
            mention_roles: Vec::new(),
        }
    }
}

/// Wire activity inside a `PRESENCE_UPDATE` or `READY_SUPPLEMENTAL`.
#[derive(Debug, Clone, Deserialize)]
pub struct WireActivity {
    /// Numeric Discord activity type (`0` = Playing .. `5` = Competing).
    #[serde(rename = "type", default)]
    pub kind: u8,
    /// Primary activity name.
    #[serde(default)]
    pub name: String,
    /// Optional detail line.
    #[serde(default)]
    pub details: Option<String>,
    /// Optional state line (also carries custom status text when `type == 4`).
    #[serde(default)]
    pub state: Option<String>,
}

/// Partial user stub inside a presence payload.
#[derive(Debug, Clone, Deserialize)]
pub struct WirePresenceUser {
    /// Target user Snowflake.
    pub id: Snowflake,
}

/// Wire presence update object.
#[derive(Debug, Clone, Deserialize)]
pub struct WirePresence {
    /// Target user wrapper (`PRESENCE_UPDATE`).
    #[serde(default)]
    pub user: Option<WirePresenceUser>,
    /// Flat user ID (`READY_SUPPLEMENTAL.merged_presences`).
    #[serde(default)]
    pub user_id: Option<Snowflake>,
    /// Presence string (`"online"`, `"idle"`, `"dnd"`, `"invisible"`, `"offline"`).
    #[serde(default)]
    pub status: String,
    /// Active activities.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub activities: Vec<WireActivity>,
}

impl WirePresence {
    /// Convert into [`Presence`], returning `None` if neither `user.id` nor `user_id` is present.
    #[must_use]
    pub fn into_core(self) -> Option<Presence> {
        let user_id = self.user.map(|u| u.id).or(self.user_id)?;
        let status = match self.status.as_str() {
            "online" => PresenceStatus::Online,
            "idle" => PresenceStatus::Idle,
            "dnd" => PresenceStatus::DoNotDisturb,
            "invisible" => PresenceStatus::Invisible,
            _ => PresenceStatus::Offline,
        };
        let mut custom_status = None;
        let mut activities = Vec::with_capacity(self.activities.len());
        for act in self.activities {
            let kind = match act.kind {
                0 => ActivityKind::Playing,
                1 => ActivityKind::Streaming,
                2 => ActivityKind::Listening,
                3 => ActivityKind::Watching,
                4 => {
                    if custom_status.is_none() {
                        custom_status = act.state.clone().filter(|s| !s.is_empty());
                    }
                    ActivityKind::Custom
                }
                5 => ActivityKind::Competing,
                _ => ActivityKind::Playing,
            };
            activities.push(Activity {
                kind,
                name: act.name,
                details: act.details,
                state: act.state,
            });
        }
        Some(Presence {
            user_id,
            status,
            custom_status,
            activities,
        })
    }
}

/// Wire Discord voice state payload (`VOICE_STATE_UPDATE` and `READY` initial voice states).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireVoiceState {
    /// Owning guild Snowflake (`None` for 1:1 DM and Group DM calls).
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    /// Connected voice or DM/Group DM channel Snowflake (`None` when disconnecting).
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    /// Participant user Snowflake.
    pub user_id: Snowflake,
    /// Voice session ID correlated with `VOICE_SERVER_UPDATE`.
    #[serde(default)]
    pub session_id: String,
    /// Server-enforced deafen.
    #[serde(default)]
    pub deaf: bool,
    /// Server-enforced mute.
    #[serde(default)]
    pub mute: bool,
    /// User self-deafen.
    #[serde(default)]
    pub self_deaf: bool,
    /// User self-mute.
    #[serde(default)]
    pub self_mute: bool,
    /// User broadcasting Go Live stream.
    #[serde(default)]
    pub self_stream: Option<bool>,
    /// User transmitting camera video.
    #[serde(default)]
    pub self_video: bool,
    /// Stage suppress flag.
    #[serde(default)]
    pub suppress: bool,
    /// Optional member object with user/nick metadata.
    #[serde(default)]
    pub member: Option<WireGuildMember>,
}

/// Partial guild member object nested inside voice states or typing events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireGuildMember {
    /// Optional guild nickname.
    #[serde(default)]
    pub nick: Option<String>,
    /// Optional user profile.
    #[serde(default)]
    pub user: Option<WireUser>,
}

impl WireGuildMember {
    /// Preferred display name (`nick` -> `user.global_name` -> `user.username`).
    #[must_use]
    pub fn display_name(&self) -> Option<String> {
        if let Some(nick) = self.nick.as_deref().filter(|s| !s.is_empty()) {
            return Some(nick.to_owned());
        }
        self.user.as_ref().map(|u| {
            u.global_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&u.username)
                .to_owned()
        })
    }
}

/// Wire response from `POST /api/v10/attachments/refresh-urls`.
#[derive(Debug, Clone, Deserialize)]
pub struct WireRefreshUrlsResponse {
    /// Refreshed URL entries.
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub refreshed_urls: Vec<WireRefreshedUrlEntry>,
}

/// Single entry in `refreshed_urls`.
#[derive(Debug, Clone, Deserialize)]
pub struct WireRefreshedUrlEntry {
    /// Original URL requested.
    #[serde(default)]
    pub original: String,
    /// Fresh signed CDN URL.
    pub refreshed: String,
}

impl From<WireRefreshedUrlEntry> for RefreshedAttachmentUrl {
    fn from(entry: WireRefreshedUrlEntry) -> Self {
        let expires_at_unix = extract_cdn_expiry_unix(&entry.refreshed);
        Self {
            original_url: entry.original,
            refreshed_url: entry.refreshed,
            expires_at_unix,
        }
    }
}

/// Discord REST API error envelope (including HTTP 429 rate-limit fields).
#[derive(Debug, Clone, Deserialize)]
pub struct WireApiError {
    /// Optional Discord error code.
    #[serde(default)]
    pub code: Option<u32>,
    /// Human-readable error message.
    #[serde(default)]
    pub message: Option<String>,
    /// Rate-limit retry delay in seconds (HTTP 429).
    #[serde(default)]
    pub retry_after: Option<f64>,
    /// Whether a HTTP 429 hit the global rate limit.
    #[serde(default)]
    pub global: Option<bool>,
}
