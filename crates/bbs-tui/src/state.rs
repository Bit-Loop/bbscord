//! Client-side store fed by [`BackendEvent`]s: guilds, channels, messages, typing, voice, calls.

use std::collections::{HashMap, VecDeque};

use bbs_core::{
    BackendEvent, CallRingState, Channel, ChannelKind, ConnectionState, GifItem, Guild,
    GuildFolder, GuildMemberSummary, Message, PresenceStatus, Snowflake, StreamDescriptor, User,
    VideoFrame, VoiceChannelStatus, VoiceParticipant, VoicePlaybackState,
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
    /// Guild member profiles (nicknames + role IDs) keyed by `(guild_id -> user_id)`.
    pub guild_members: HashMap<Snowflake, HashMap<Snowflake, GuildMemberSummary>>,
    /// Presence by user.
    pub presence: HashMap<Snowflake, PresenceStatus>,
    /// Active voice session.
    pub voice: Option<VoiceChannelStatus>,
    /// Voice-channel occupant rosters synced from Gateway voice state events.
    pub voice_occupants: HashMap<Snowflake, Vec<VoiceParticipant>>,
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
    /// Latest GIF search error `(request_id, sanitized_message)`.
    pub gif_error: Option<(u64, String)>,
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

    /// Sum of unread across DMs (muted DMs contribute mentions only).
    #[must_use]
    pub fn dm_unread(&self) -> (u32, u32) {
        self.dms.iter().fold((0, 0), |(u, m), c| {
            let add_u = if c.muted { 0 } else { c.unread_count };
            (u.saturating_add(add_u), m.saturating_add(c.mention_count))
        })
    }

    /// Whether a channel is effectively muted (either the channel itself or its owning guild).
    #[must_use]
    pub fn is_channel_muted(&self, channel_id: Snowflake) -> bool {
        let Some(c) = self.channel(channel_id) else {
            return false;
        };
        if c.muted {
            return true;
        }
        c.guild_id
            .and_then(|gid| self.guild(gid))
            .is_some_and(|g| g.muted)
    }

    /// Guild-specific nickname for a user if known.
    #[must_use]
    pub fn member_nick(&self, guild_id: Snowflake, user_id: Snowflake) -> Option<&str> {
        self.guild_members
            .get(&guild_id)?
            .get(&user_id)?
            .nick
            .as_deref()
            .filter(|s| !s.is_empty())
    }

    /// Best display name for a message author (message nick > guild member nick > global/username).
    #[must_use]
    pub fn author_display_name(&self, msg: &Message) -> String {
        if let Some(nick) = msg.author_nick.as_deref().filter(|s| !s.is_empty()) {
            return nick.to_string();
        }
        if let Some(gid) = msg.guild_id
            && let Some(nick) = self.member_nick(gid, msg.author.id)
        {
            return nick.to_string();
        }
        msg.author.display_name().to_string()
    }

    /// Highest-position non-zero 24-bit RGB role color for a guild member.
    #[must_use]
    pub fn member_role_color(
        &self,
        guild_id: Snowflake,
        user_id: Snowflake,
        msg_role_ids: &[Snowflake],
    ) -> Option<u32> {
        let guild = self.guild(guild_id)?;
        if !msg_role_ids.is_empty()
            && let Some(color) = guild.highest_role_color(msg_role_ids)
        {
            return Some(color);
        }
        let member = self.guild_members.get(&guild_id)?.get(&user_id)?;
        guild.highest_role_color(&member.role_ids)
    }

    /// Role name in a guild by role id.
    #[must_use]
    pub fn role_name(&self, guild_id: Snowflake, role_id: Snowflake) -> Option<&str> {
        self.guild(guild_id)?
            .roles
            .iter()
            .find(|r| r.id == role_id)
            .map(|r| r.name.as_str())
    }

    /// Voice occupants for a channel, merging Gateway voice roster with live local voice status.
    #[must_use]
    pub fn effective_voice_occupants(&self, channel_id: Snowflake) -> Vec<VoiceParticipant> {
        let mut out = self
            .voice_occupants
            .get(&channel_id)
            .cloned()
            .unwrap_or_default();
        if let Some(v) = &self.voice
            && v.channel_id == channel_id
        {
            for vp in &v.participants {
                if let Some(existing) = out.iter_mut().find(|p| p.user_id == vp.user_id) {
                    *existing = vp.clone();
                } else {
                    out.push(vp.clone());
                }
            }
        }
        out
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

    /// Whether a message mentions the signed-in user (honoring guild suppression settings).
    #[must_use]
    pub fn mentions_me(&self, m: &Message) -> bool {
        let Some(me) = self.me.as_ref() else {
            return m.mention_everyone;
        };
        if m.mentions.contains(&me.id) {
            return true;
        }
        let guild = m.guild_id.and_then(|gid| self.guild(gid));
        if m.mention_everyone && !guild.is_some_and(|g| g.suppress_everyone) {
            return true;
        }
        if !m.mention_roles.is_empty()
            && !guild.is_some_and(|g| g.suppress_roles)
            && let Some(gid) = m.guild_id
            && let Some(member) = self.guild_members.get(&gid).and_then(|hm| hm.get(&me.id))
            && member
                .role_ids
                .iter()
                .any(|rid| m.mention_roles.contains(rid))
        {
            return true;
        }
        false
    }

    fn remember_user(&mut self, u: &User) {
        self.users.entry(u.id).or_insert_with(|| u.clone());
    }

    fn remember_member_from_message(&mut self, m: &Message) {
        let Some(gid) = m.guild_id else {
            return;
        };
        if m.author_nick.is_none() && m.author_role_ids.is_empty() {
            return;
        }
        let entry = self
            .guild_members
            .entry(gid)
            .or_default()
            .entry(m.author.id)
            .or_insert_with(|| GuildMemberSummary {
                user_id: m.author.id,
                nick: None,
                role_ids: Vec::new(),
            });
        if let Some(nick) = &m.author_nick {
            entry.nick = Some(nick.clone());
        }
        if !m.author_role_ids.is_empty() {
            entry.role_ids = m.author_role_ids.clone();
        }
    }

    /// Recompute a guild's aggregate `unread` and `mention_count` from its loaded channels.
    pub fn recompute_guild_unread(&mut self, gid: Snowflake) {
        let Some(chs) = self.channels.get(&gid) else {
            return;
        };
        let guild_muted = self.guild(gid).is_some_and(|g| g.muted);
        let unread = !guild_muted && chs.iter().any(|c| !c.muted && c.unread_count > 0);
        let mentions: u32 = chs
            .iter()
            .fold(0u32, |acc, c| acc.saturating_add(c.mention_count));
        if let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid) {
            g.unread = unread;
            g.mention_count = mentions;
        }
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
        if let Some(gid) = gid {
            if self.channels.contains_key(&gid) {
                self.recompute_guild_unread(gid);
            } else if let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid) {
                if !g.muted {
                    g.unread = true;
                }
                if mention {
                    g.mention_count = g.mention_count.saturating_add(1);
                }
            }
        }
    }

    /// Mark a channel read.
    pub fn mark_read(&mut self, channel: Snowflake) {
        let gid = self.channel(channel).and_then(|c| c.guild_id);
        let latest_loaded = self
            .messages
            .get(&channel)
            .and_then(|cm| cm.messages.back().map(|m| m.id));
        if let Some(c) = self.channel_mut(channel) {
            c.unread_count = 0;
            c.mention_count = 0;
            if let Some(id) = c.last_message_id.or(latest_loaded) {
                c.last_ack_message_id = Some(id);
            }
        }
        if let Some(gid) = gid {
            self.recompute_guild_unread(gid);
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
                        if r.presence != PresenceStatus::Offline {
                            self.presence.entry(r.id).or_insert(r.presence);
                        }
                    }
                }
                self.dms = dm_channels;
                self.connection = ConnectionState::Ready;
            }
            BackendEvent::ChannelsLoaded {
                guild_id,
                mut channels,
            } => {
                struct PrevChannelState {
                    unread: u32,
                    mention: u32,
                    muted: bool,
                    last_ack: Option<Snowflake>,
                    last_msg: Option<Snowflake>,
                }
                let prev: HashMap<Snowflake, PrevChannelState> = match guild_id {
                    Some(g) => self.channels.get(&g).map(Vec::as_slice).unwrap_or(&[]),
                    None => &self.dms,
                }
                .iter()
                .map(|c| {
                    (
                        c.id,
                        PrevChannelState {
                            unread: c.unread_count,
                            mention: c.mention_count,
                            muted: c.muted,
                            last_ack: c.last_ack_message_id,
                            last_msg: c.last_message_id,
                        },
                    )
                })
                .collect();
                let had_prev = !prev.is_empty();
                for c in &mut channels {
                    for r in &c.recipients {
                        self.users.entry(r.id).or_insert_with(|| r.clone());
                        if r.presence != PresenceStatus::Offline {
                            self.presence.entry(r.id).or_insert(r.presence);
                        }
                    }
                    if let Some(old) = prev.get(&c.id) {
                        if !c.muted && old.muted {
                            c.muted = true;
                        }
                        if c.last_ack_message_id.is_none() {
                            c.last_ack_message_id = old.last_ack;
                        }
                        if c.last_message_id.is_none() {
                            c.last_message_id = old.last_msg;
                        }
                        if c.mention_count == 0 && old.mention > 0 {
                            c.mention_count = old.mention;
                        }
                        let acked_to_latest = match (c.last_ack_message_id, c.last_message_id) {
                            (Some(ack), Some(last)) => ack.as_u64() >= last.as_u64(),
                            _ => false,
                        };
                        if c.unread_count == 0 && old.unread > 0 && !acked_to_latest {
                            c.unread_count = old.unread;
                        }
                    }
                    if c.unread_count == 0
                        && let (Some(ack), Some(last)) = (c.last_ack_message_id, c.last_message_id)
                        && last.as_u64() > ack.as_u64()
                    {
                        c.unread_count = 1;
                    }
                }
                channels.sort_by_key(|c| (c.position, c.id.as_u64()));
                match guild_id {
                    Some(g) => {
                        let any_signal = channels
                            .iter()
                            .any(|c| c.unread_count > 0 || c.mention_count > 0);
                        self.channels.insert(g, channels);
                        if had_prev || any_signal {
                            self.recompute_guild_unread(g);
                        }
                    }
                    None => self.dms = channels,
                }
            }
            BackendEvent::ChannelUpserted(mut ch) => {
                for r in &ch.recipients {
                    self.users.entry(r.id).or_insert_with(|| r.clone());
                }
                if let Some(existing) = self.channel_mut(ch.id) {
                    if !ch.muted && existing.muted {
                        ch.muted = true;
                    }
                    if ch.last_ack_message_id.is_none() {
                        ch.last_ack_message_id = existing.last_ack_message_id;
                    }
                    if ch.last_message_id.is_none() {
                        ch.last_message_id = existing.last_message_id;
                    }
                    if ch.unread_count == 0 && existing.unread_count > 0 {
                        ch.unread_count = existing.unread_count;
                    }
                    if ch.mention_count == 0 && existing.mention_count > 0 {
                        ch.mention_count = existing.mention_count;
                    }
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
                        self.recompute_guild_unread(g);
                    }
                    None => self.dms.retain(|c| c.id != channel_id),
                }
                self.messages.remove(&channel_id);
                self.voice_occupants.remove(&channel_id);
            }
            BackendEvent::MessagesLoaded {
                channel_id,
                messages,
                has_more_before,
            } => {
                for m in &messages {
                    self.remember_user(&m.author);
                    self.remember_member_from_message(m);
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
                self.remember_member_from_message(&m);
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
                self.remember_user(&m.author);
                self.remember_member_from_message(&m);
                if let Some(entry) = self.messages.get_mut(&m.channel_id)
                    && let Some(i) = entry.index_of(m.id)
                {
                    entry.messages[i] = m;
                }
            }
            BackendEvent::MessagePatched(patch) => {
                let mut patched_member = None;
                if let Some(entry) = self.messages.get_mut(&patch.channel_id)
                    && let Some(i) = entry.index_of(patch.id)
                {
                    entry.messages[i].apply_patch(patch);
                    if entry.messages[i].guild_id.is_some()
                        && (entry.messages[i].author_nick.is_some()
                            || !entry.messages[i].author_role_ids.is_empty())
                    {
                        patched_member = Some(entry.messages[i].clone());
                    }
                }
                if let Some(m) = patched_member {
                    self.remember_member_from_message(&m);
                }
            }
            BackendEvent::MessageAcked {
                channel_id,
                last_message_id,
                mention_count,
            } => {
                let gid = self.channel(channel_id).and_then(|c| c.guild_id);
                if let Some(c) = self.channel_mut(channel_id) {
                    c.last_ack_message_id = Some(last_message_id);
                    c.mention_count = mention_count;
                    if c.last_message_id
                        .is_none_or(|last| last_message_id.as_u64() >= last.as_u64())
                    {
                        c.unread_count = 0;
                    }
                }
                if let Some(gid) = gid {
                    self.recompute_guild_unread(gid);
                }
            }
            BackendEvent::GuildSettingsUpdated {
                guild_id,
                muted,
                suppress_everyone,
                suppress_roles,
                muted_channels,
            } => {
                if let Some(gid) = guild_id
                    && let Some(g) = self.guilds.iter_mut().find(|g| g.id == gid)
                {
                    g.muted = muted;
                    g.suppress_everyone = suppress_everyone;
                    g.suppress_roles = suppress_roles;
                }
                for (cid, cmuted) in muted_channels {
                    if let Some(c) = self.channel_mut(cid) {
                        c.muted = cmuted;
                    }
                }
                if let Some(gid) = guild_id {
                    self.recompute_guild_unread(gid);
                }
            }
            BackendEvent::GuildMembersUpdated { guild_id, members } => {
                let map = self.guild_members.entry(guild_id).or_default();
                for m in members {
                    map.insert(m.user_id, m);
                }
            }
            BackendEvent::GuildRolesUpdated { guild_id, roles } => {
                if let Some(g) = self.guilds.iter_mut().find(|g| g.id == guild_id) {
                    g.roles = roles;
                }
            }
            BackendEvent::GifSearchFailed { request_id, error } => {
                self.gif_error = Some((
                    request_id,
                    crate::sanitize::sanitize_single_line(&error).into_owned(),
                ));
            }
            BackendEvent::VoiceRosterUpdated {
                channel_id,
                participants,
                ..
            } => {
                if participants.is_empty() {
                    self.voice_occupants.remove(&channel_id);
                } else {
                    self.voice_occupants.insert(channel_id, participants);
                }
            }
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
                self.gif_error = None;
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
