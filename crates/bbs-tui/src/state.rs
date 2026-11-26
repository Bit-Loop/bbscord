//! Client-side store fed by [`BackendEvent`]s: guilds, channels, messages, typing, voice, calls.

use std::collections::{HashMap, VecDeque};

use bbs_core::{
    BackendEvent, CallRingState, Channel, ChannelKind, ConnectionState, GifItem, Guild,
    GuildFolder, Message, PresenceStatus, Snowflake, StreamDescriptor, User, VideoFrame,
    VoiceChannelStatus, VoicePlaybackState,
};

/// Max messages retained per channel.
pub const MAX_MESSAGES_PER_CHANNEL: usize = 2_000;
/// Latency history samples kept for the status sparkline.
pub const LATENCY_HISTORY: usize = 64;

/// A typing indicator entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typing {
    /// User.
    pub user_id: Snowflake,
    /// Display name.
    pub name: String,
    /// Expiry (unix seconds).
    pub expires_at_unix: i64,
}

/// Messages of one channel, chronological.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChannelMessages {
    /// Oldest → newest.
    pub messages: VecDeque<Message>,
    /// Older history exists.
    pub has_more_before: bool,
    /// A history page was requested and not yet answered.
    pub loading: bool,
}

impl ChannelMessages {
    /// Index of a message.
    #[must_use]
    pub fn index_of(&self, id: Snowflake) -> Option<usize> {
        self.messages.iter().position(|m| m.id == id)
    }
}

/// Error banner shown in the status area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    /// Scope.
    pub scope: String,
    /// Sanitized text.
    pub message: String,
    /// Recoverable.
    pub recoverable: bool,
}

/// Everything the UI knows about the account.
#[derive(Debug, Default)]
pub struct Store {
    /// Signed-in user.
    pub me: Option<User>,
    /// Gateway state.
    pub connection: ConnectionState,
    /// Gateway latency history (ms).
    pub latency: VecDeque<u32>,
    /// Guilds in server order.
    pub guilds: Vec<Guild>,
    /// Folder layout.
    pub folders: Vec<GuildFolder>,
    /// DM + Group DM channels.
    pub dms: Vec<Channel>,
    /// Channels per guild.
    pub channels: HashMap<Snowflake, Vec<Channel>>,
    /// Messages per channel.
    pub messages: HashMap<Snowflake, ChannelMessages>,
    /// Typing per channel.
    pub typing: HashMap<Snowflake, Vec<Typing>>,
    /// Known users.
    pub users: HashMap<Snowflake, User>,
    /// Presence by user.
    pub presence: HashMap<Snowflake, PresenceStatus>,
    /// Active voice session.
    pub voice: Option<VoiceChannelStatus>,
    /// Call ring state.
    pub ring: CallRingState,
    /// Streams per voice channel.
    pub streams: HashMap<Snowflake, Vec<StreamDescriptor>>,
    /// Latest inline video frame.
    pub video_frame: Option<VideoFrame>,
    /// Voice-message playback.
    pub playback: Option<VoicePlaybackState>,
    /// Latest GIF results `(request_id, items, next_cursor)`.
    pub gif_results: Option<(u64, Vec<GifItem>, Option<String>)>,
    /// Error banners (newest last).
    pub banners: Vec<Banner>,
    /// Count of events applied (diagnostics).
    pub events_applied: u64,
}

impl Store {
    /// Look up any channel by id.
    #[must_use]
    pub fn channel(&self, id: Snowflake) -> Option<&Channel> {
        self.dms
            .iter()
            .find(|c| c.id == id)
            .or_else(|| self.channels.values().flatten().find(|c| c.id == id))
    }

    fn channel_mut(&mut self, id: Snowflake) -> Option<&mut Channel> {
        if let Some(c) = self.dms.iter_mut().find(|c| c.id == id) {
            return Some(c);
        }
        self.channels.values_mut().flatten().find(|c| c.id == id)
    }

    /// Guild by id.
    #[must_use]
    pub fn guild(&self, id: Snowflake) -> Option<&Guild> {
        self.guilds.iter().find(|g| g.id == id)
    }

    /// Guilds in folder order (folders flattened; unlisted guilds appended).
    #[must_use]
    pub fn ordered_guilds(&self) -> Vec<&Guild> {
        let mut out: Vec<&Guild> = Vec::new();
        for f in &self.folders {
            for gid in &f.guild_ids {
                if let Some(g) = self.guild(*gid)
                    && !out.iter().any(|x| x.id == g.id)
                {
                    out.push(g);
                }
            }
        }
        for g in &self.guilds {
            if !out.iter().any(|x| x.id == g.id) {
                out.push(g);
            }
        }
        out
    }

    /// DMs sorted by recency (newest first).
    #[must_use]
    pub fn dms_by_recency(&self) -> Vec<&Channel> {
        let mut v: Vec<&Channel> = self.dms.iter().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.last_message_id.map_or(0, Snowflake::as_u64)));
        v
    }

    /// Sum of unread across DMs.
    #[must_use]
    pub fn dm_unread(&self) -> (u32, u32) {
        self.dms.iter().fold((0, 0), |(u, m), c| {
            (u + c.unread_count, m + c.mention_count)
        })
    }

    /// Display name for a user id.
    #[must_use]
    pub fn user_name(&self, id: Snowflake) -> Option<String> {
        self.users.get(&id).map(|u| u.display_name().to_string())
    }

    /// Active typing names for a channel at `now_unix`.
    #[must_use]
    pub fn typing_names(&self, channel: Snowflake, now_unix: i64) -> Vec<String> {
        let me = self.me.as_ref().map(|u| u.id);
        self.typing
            .get(&channel)
            .map(|v| {
                v.iter()
                    .filter(|t| t.expires_at_unix >= now_unix && Some(t.user_id) != me)
                    .map(|t| t.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether a message mentions the signed-in user.
    #[must_use]
    pub fn mentions_me(&self, m: &Message) -> bool {
        self.me
            .as_ref()
            .is_some_and(|me| m.mentions.contains(&me.id))
            || m.mention_everyone
    }

    fn remember_user(&mut self, u: &User) {
        self.users.entry(u.id).or_insert_with(|| u.clone());
    }

    fn bump_unread(&mut self, msg: &Message, active: Option<Snowflake>) {
        if Some(msg.channel_id) == active
            || self.me.as_ref().is_some_and(|me| me.id == msg.author.id)
        {
            return;
        }
        let mention = self.mentions_me(msg) || msg.guild_id.is_none();
        let gid = msg.guild_id;
        if let Some(c) = self.channel_mut(msg.channel_id) {
            c.unread_count = c.unread_count.saturating_add(1);
            if mention {
                c.mention_count = c.mention_count.saturating_add(1);
            }
            c.last_message_id = Some(msg.id);
        }
        if let Some(gid) = gid
            && let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid)
        {
            g.unread = true;
            if mention {
                g.mention_count = g.mention_count.saturating_add(1);
            }
        }
    }

    /// Mark a channel read.
    pub fn mark_read(&mut self, channel: Snowflake) {
        let gid = self.channel(channel).and_then(|c| c.guild_id);
        if let Some(c) = self.channel_mut(channel) {
            c.unread_count = 0;
            c.mention_count = 0;
        }
        if let Some(gid) = gid {
            let (unread, mentions) = self.channels.get(&gid).map_or((false, 0), |chs| {
                (
                    chs.iter().any(|c| c.unread_count > 0),
                    chs.iter().map(|c| c.mention_count).sum(),
                )
            });
            if let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid) {
                g.unread = unread;
                g.mention_count = mentions;
            }
        }
    }

    /// Apply a backend event. `active` is the channel on screen (no unread bump).
    pub fn apply(&mut self, ev: BackendEvent, active: Option<Snowflake>) {
        self.events_applied = self.events_applied.wrapping_add(1);
        match ev {
            BackendEvent::ConnectionChanged {
                state, latency_ms, ..
            } => {
                self.connection = state;
                if let Some(l) = latency_ms {
                    self.latency.push_back(l);
                    while self.latency.len() > LATENCY_HISTORY {
                        self.latency.pop_front();
                    }
                }
            }
            BackendEvent::Ready {
                current_user,
                guilds,
                guild_folders,
                dm_channels,
            } => {
                self.remember_user(&current_user);
                self.me = Some(current_user);
                self.guilds = guilds;
                self.folders = guild_folders;
                for c in &dm_channels {
                    for r in &c.recipients {
                        self.users.entry(r.id).or_insert_with(|| r.clone());
                    }
                }
                self.dms = dm_channels;
                self.connection = ConnectionState::Ready;
            }
            BackendEvent::ChannelsLoaded {
                guild_id,
                mut channels,
            } => {
                channels.sort_by_key(|c| (c.position, c.id.as_u64()));
                match guild_id {
                    Some(g) => {
                        self.channels.insert(g, channels);
                    }
                    None => self.dms = channels,
                }
            }
            BackendEvent::ChannelUpserted(ch) => {
                if let Some(existing) = self.channel_mut(ch.id) {
                    *existing = ch;
                } else if let Some(g) = ch.guild_id {
                    let v = self.channels.entry(g).or_default();
                    v.push(ch);
                    v.sort_by_key(|c| (c.position, c.id.as_u64()));
                } else {
                    self.dms.push(ch);
                }
            }
            BackendEvent::ChannelDeleted {
                guild_id,
                channel_id,
            } => {
                match guild_id {
                    Some(g) => {
                        if let Some(v) = self.channels.get_mut(&g) {
                            v.retain(|c| c.id != channel_id);
                        }
                    }
                    None => self.dms.retain(|c| c.id != channel_id),
                }
                self.messages.remove(&channel_id);
            }
            BackendEvent::MessagesLoaded {
                channel_id,
                messages,
                has_more_before,
            } => {
                for m in &messages {
                    self.remember_user(&m.author);
                }
                let entry = self.messages.entry(channel_id).or_default();
                entry.loading = false;
                entry.has_more_before = has_more_before;
                // Merge: prepend older page, dedupe by id, keep chronological.
                let mut merged: Vec<Message> = messages;
                for m in entry.messages.drain(..) {
                    if !merged.iter().any(|x| x.id == m.id) {
                        merged.push(m);
                    }
                }
                merged.sort_by_key(|m| m.id.as_u64());
                let excess = merged.len().saturating_sub(MAX_MESSAGES_PER_CHANNEL);
                entry.messages = merged.into_iter().skip(excess).collect();
            }
            BackendEvent::MessageCreated(m) => {
                self.remember_user(&m.author);
                self.bump_unread(&m, active);
                if let Some(t) = self.typing.get_mut(&m.channel_id) {
                    t.retain(|t| t.user_id != m.author.id);
                }
                let entry = self.messages.entry(m.channel_id).or_default();
                if let Some(nonce) = m.nonce.as_deref()
                    && let Some(pos) = entry
                        .messages
                        .iter()
                        .position(|x| x.nonce.as_deref() == Some(nonce))
                {
                    entry.messages[pos] = m;
                    return;
                }
                if entry.messages.iter().any(|x| x.id == m.id) {
                    return;
                }
                let pos = entry
                    .messages
                    .iter()
                    .rposition(|x| x.id.as_u64() < m.id.as_u64());
                match pos {
                    Some(p) => entry.messages.insert(p + 1, m),
                    None => entry.messages.push_front(m),
                }
                while entry.messages.len() > MAX_MESSAGES_PER_CHANNEL {
                    entry.messages.pop_front();
                }
            }
            BackendEvent::MessageUpdated(m) => {
                if let Some(entry) = self.messages.get_mut(&m.channel_id)
                    && let Some(i) = entry.index_of(m.id)
                {
                    entry.messages[i] = m;
                }
            }
            BackendEvent::MessagePatched(patch) => {
                if let Some(entry) = self.messages.get_mut(&patch.channel_id)
                    && let Some(i) = entry.index_of(patch.id)
                {
                    entry.messages[i].apply_patch(patch);
                }
            }
            BackendEvent::MessageAcked { .. }
            | BackendEvent::GuildSettingsUpdated { .. }
            | BackendEvent::GuildMembersUpdated { .. }
            | BackendEvent::GuildRolesUpdated { .. }
            | BackendEvent::GifSearchFailed { .. }
            | BackendEvent::VoiceRosterUpdated { .. } => {}
            BackendEvent::MessageDeleted {
                channel_id,
                message_id,
            } => {
                if let Some(entry) = self.messages.get_mut(&channel_id) {
                    entry.messages.retain(|m| m.id != message_id);
                }
            }
            BackendEvent::ReactionAdded {
                channel_id,
                message_id,
                reaction,
            } => {
                if let Some(m) = self.find_message_mut(channel_id, message_id) {
                    if let Some(r) = m.reactions.iter_mut().find(|r| {
                        r.emoji_name == reaction.emoji_name && r.emoji_id == reaction.emoji_id
                    }) {
                        *r = reaction;
                    } else {
                        m.reactions.push(reaction);
                    }
                }
            }
            BackendEvent::ReactionRemoved {
                channel_id,
                message_id,
                emoji_id,
                emoji_name,
                by_me,
            } => {
                if let Some(m) = self.find_message_mut(channel_id, message_id) {
                    if let Some(r) = m
                        .reactions
                        .iter_mut()
                        .find(|r| r.emoji_name == emoji_name && r.emoji_id == emoji_id)
                    {
                        r.count = r.count.saturating_sub(1);
                        if by_me {
                            r.me = false;
                        }
                    }
                    m.reactions.retain(|r| r.count > 0);
                }
            }
            BackendEvent::TypingStarted {
                channel_id,
                user_id,
                display_name,
                expires_at_unix,
            } => {
                let v = self.typing.entry(channel_id).or_default();
                v.retain(|t| t.user_id != user_id);
                v.push(Typing {
                    user_id,
                    name: display_name,
                    expires_at_unix,
                });
            }
            BackendEvent::PresenceUpdated(p) => {
                self.presence.insert(p.user_id, p.status);
                if let Some(u) = self.users.get_mut(&p.user_id) {
                    u.presence = p.status;
                }
            }
            BackendEvent::GifSearchResults { request_id, page } => {
                self.gif_results = Some((request_id, page.items, page.next_cursor));
            }
            BackendEvent::VoiceStatusUpdated(s) => {
                if s.connection_state == bbs_core::VoiceConnectionState::Disconnected {
                    self.voice = None;
                } else {
                    self.ring = s.call_ring.clone();
                    self.voice = Some(s);
                }
            }
            BackendEvent::VoiceAudioLevels { channel_id, levels } => {
                if let Some(v) = self.voice.as_mut()
                    && v.channel_id == channel_id
                {
                    for (uid, db, speaking) in levels {
                        if let Some(p) = v.participants.iter_mut().find(|p| p.user_id == uid) {
                            p.rms_db = if db.is_finite() {
                                db.clamp(-60.0, 0.0)
                            } else {
                                -60.0
                            };
                            p.speaking = speaking;
                        }
                    }
                }
            }
            BackendEvent::VoicePlaybackUpdated(p) => {
                self.playback = if p.playing || p.position_secs > 0.0 {
                    Some(p)
                } else {
                    None
                };
            }
            BackendEvent::CallRingUpdated(r) => self.ring = r,
            BackendEvent::ActiveStreamsUpdated {
                channel_id,
                streams,
            } => {
                self.streams.insert(channel_id, streams);
            }
            BackendEvent::VideoFrameReceived(f) => self.video_frame = Some(f),
            BackendEvent::AttachmentUrlRefreshed {
                channel_id,
                message_id,
                attachment_id,
                url,
                expires_at_unix,
            } => {
                if let Some(m) = self.find_message_mut(channel_id, message_id)
                    && let Some(a) = m.attachments.iter_mut().find(|a| a.id == attachment_id)
                {
                    a.url = url;
                    a.expires_at_unix = expires_at_unix;
                }
            }
            BackendEvent::ErrorBanner {
                scope,
                message,
                recoverable,
            } => {
                self.banners.push(Banner {
                    scope: crate::sanitize::sanitize_single_line(&scope).into_owned(),
                    message: crate::sanitize::sanitize_single_line(&message).into_owned(),
                    recoverable,
                });
                if self.banners.len() > 8 {
                    self.banners.remove(0);
                }
            }
        }
    }

    fn find_message_mut(&mut self, channel: Snowflake, id: Snowflake) -> Option<&mut Message> {
        self.messages
            .get_mut(&channel)?
            .messages
            .iter_mut()
            .find(|m| m.id == id)
    }

    /// Text channels of a guild in sidebar order (categories followed by children).
    #[must_use]
    pub fn sidebar_channels(&self, guild: Snowflake) -> Vec<&Channel> {
        let Some(chs) = self.channels.get(&guild) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let top: Vec<&Channel> = chs
            .iter()
            .filter(|c| c.parent_id.is_none() && c.kind != ChannelKind::GuildCategory)
            .collect();
        out.extend(top);
        for cat in chs.iter().filter(|c| c.kind == ChannelKind::GuildCategory) {
            out.push(cat);
            for c in chs
                .iter()
                .filter(|c| c.parent_id == Some(cat.id) && !c.kind.is_thread())
            {
                out.push(c);
                for t in chs
                    .iter()
                    .filter(|t| t.parent_id == Some(c.id) && t.kind.is_thread())
                {
                    out.push(t);
                }
            }
        }
        // Threads whose parent is top-level.
        for c in chs.iter().filter(|c| c.kind.is_thread()) {
            if !out.iter().any(|x| x.id == c.id) {
                out.push(c);
            }
        }
        out
    }
}
