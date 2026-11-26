//! The application model: store + UI state, event application, and the 4-tier input router (§9).

use std::collections::{HashMap, HashSet};

use bbs_core::{
    BackendEvent, CallRingState, ChannelKind, GifSearchQuery, Message, RgbaFrame, ScrollState,
    Snowflake, StreamQuality, StreamSourceKind, UiAction, VideoSinkKind, ViewportGeometry,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::charset::Glyphs;
use crate::composer::{ComposeTarget, Composer};
use crate::config::{CtrlKMode, TuiConfig};
use crate::geometry::{Layout, LayoutMemory};
use crate::micro::MicroState;
use crate::modals::{self, Modal, ModalOutcome};
use crate::scroll::{LayoutCache, Scroll};
use crate::state::Store;
use crate::theme::{self, Theme};

/// Side effects the runtime must perform.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Forward to controllers/backends.
    Action(UiAction),
    /// Full repaint: re-emit terminal modes (DECAWM off, paste, kbd flags), re-place images.
    Redraw,
    /// Suspend (`Ctrl+Z`): run teardown, `raise(SIGTSTP)`, restore on `SIGCONT`.
    Suspend,
    /// Exit the program.
    Quit,
    /// Open `$EDITOR` with this text; feed the result back via [`App::set_composer_text`].
    ExternalEditor(String),
    /// Send oversized text as a `message.txt` attachment.
    SendAsFile {
        /// Channel.
        channel_id: Snowflake,
        /// Text.
        content: String,
        /// Reply target.
        reply_to: Option<Snowflake>,
    },
    /// Audible bell.
    Bell,
}

/// Editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// BBS hotkeys.
    #[default]
    Normal,
    /// Composer has focus.
    Insert,
}

/// Focused pane in NORMAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Guild/channel tree.
    Sidebar,
    /// Message list.
    #[default]
    Messages,
    /// Members list.
    Members,
    /// Ultra deck `0..=2`.
    Deck(u8),
}

/// Sidebar row kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideItem {
    /// DMs pseudo-guild.
    Home,
    /// A guild.
    Guild(Snowflake),
    /// A channel (with indent depth).
    Channel(Snowflake, u8),
}

/// Sidebar tree state.
#[derive(Debug, Clone, Default)]
pub struct Sidebar {
    /// Cursor row.
    pub cursor: usize,
    /// Scroll offset.
    pub offset: usize,
    /// Expanded guilds (`None` = Home).
    pub expanded: HashSet<Option<Snowflake>>,
    /// Collapsed categories.
    pub collapsed: HashSet<Snowflake>,
}

/// Decoded image cache entry provided by the media runtime (used for HalfBlock drawing).
pub type ImageStore = HashMap<String, RgbaFrame>;

/// An image box placed on screen this frame, for the graphics runtime to overlay (§6, I8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSlot {
    /// Media URL.
    pub url: String,
    /// Full box rect in cells (may extend beyond `visible`).
    pub rect: ratatui::layout::Rect,
    /// Visible (clipped) part.
    pub visible: ratatui::layout::Rect,
    /// Rows of the box clipped off the top.
    pub clip_top: u16,
    /// Animated media (only the focused one animates, §6.3).
    pub animated: bool,
    /// This slot has focus (may animate).
    pub focused: bool,
}

/// The whole UI.
pub struct App {
    /// Data store.
    pub store: Store,
    /// Config.
    pub cfg: TuiConfig,
    pub(crate) theme: Theme,
    pub(crate) theme_before_preview: Option<Theme>,
    pub(crate) mem: LayoutMemory,
    pub(crate) layout: Option<Layout>,
    pub(crate) cache: LayoutCache,
    /// Mode.
    pub mode: Mode,
    /// Focus.
    pub focus: Focus,
    /// Composer.
    pub composer: Composer,
    pub(crate) modals: Vec<Modal>,
    /// Active guild (`None` = DMs home).
    pub active_guild: Option<Snowflake>,
    /// Active channel.
    pub active_channel: Option<Snowflake>,
    pub(crate) scrolls: HashMap<Snowflake, Scroll>,
    /// Selected message.
    pub selected: Option<Snowflake>,
    pub(crate) sidebar: Sidebar,
    pub(crate) members_cursor: usize,
    pub(crate) deck_cursor: usize,
    /// Members pane enabled.
    pub show_members: bool,
    /// Sidebar enabled.
    pub show_sidebar: bool,
    pub(crate) revealed: HashSet<Snowflake>,
    pub(crate) dave_verified: HashSet<Snowflake>,
    pub(crate) micro: MicroState,
    pub(crate) now_ms: u64,
    pub(crate) gif_req: u64,
    pub(crate) ctrl_x: bool,
    pub(crate) last_ctrl_c: u64,
    pub(crate) size: (u16, u16),
    /// Decoded images for HalfBlock rendering.
    pub images: ImageStore,
    pub(crate) image_slots: Vec<ImageSlot>,
    /// Local UTC offset in minutes (set by the runtime).
    pub utc_offset_min: i32,
    pub(crate) watching: Option<String>,
    pub(crate) broadcasting: bool,
    pub(crate) pending: Vec<Effect>,
    /// Theme search directories (rescanned whenever the picker opens, §8.1).
    pub theme_dirs: theme::ThemeDirs,
    pub(crate) catalog: theme::ThemeCatalog,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("mode", &self.mode)
            .field("focus", &self.focus)
            .field("active_channel", &self.active_channel)
            .finish_non_exhaustive()
    }
}

fn ctrl(k: &KeyEvent, c: char) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char(c)
}

fn plain(k: &KeyEvent) -> Option<char> {
    match k.code {
        KeyCode::Char(c)
            if !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(c)
        }
        _ => None,
    }
}

impl App {
    /// Create an app with a config.
    #[must_use]
    pub fn new(cfg: TuiConfig) -> Self {
        let spec = theme::builtin_by_name(&cfg.theme)
            .or_else(|| theme::builtin_specs().into_iter().next())
            .unwrap_or_default();
        let theme = Theme::new(spec, cfg.depth);
        let mut sidebar = Sidebar::default();
        sidebar.expanded.insert(None);
        Self {
            store: Store::default(),
            theme,
            theme_before_preview: None,
            mem: LayoutMemory::default(),
            layout: None,
            cache: LayoutCache::default(),
            mode: Mode::Normal,
            focus: Focus::Messages,
            composer: Composer::default(),
            modals: Vec::new(),
            active_guild: None,
            active_channel: None,
            scrolls: HashMap::new(),
            selected: None,
            sidebar,
            members_cursor: 0,
            deck_cursor: 0,
            show_members: true,
            show_sidebar: true,
            revealed: HashSet::new(),
            dave_verified: HashSet::new(),
            micro: MicroState::default(),
            now_ms: 0,
            gif_req: 0,
            ctrl_x: false,
            last_ctrl_c: 0,
            size: (0, 0),
            images: HashMap::new(),
            image_slots: Vec::new(),
            utc_offset_min: 0,
            watching: None,
            broadcasting: false,
            pending: Vec::new(),
            theme_dirs: theme::ThemeDirs::from_env(),
            catalog: theme::ThemeCatalog::builtin_only(),
            cfg,
        }
    }

    /// Current theme.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Replace the theme (live repaint).
    pub fn set_theme(&mut self, spec: bbs_core::ThemeSpec) {
        self.theme = Theme::new(spec, self.cfg.depth);
    }

    /// Glyph table.
    #[must_use]
    pub fn glyphs(&self) -> Glyphs {
        Glyphs::for_tier(self.cfg.charset, self.cfg.frame)
    }

    /// Last solved layout.
    #[must_use]
    pub fn layout(&self) -> Option<&Layout> {
        self.layout.as_ref()
    }

    /// Image boxes placed by the last render.
    #[must_use]
    pub fn image_slots(&self) -> &[ImageSlot] {
        &self.image_slots
    }

    /// Advance the clock (unix ms). Drives typing expiry, auto-read and ring timeouts.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect> {
        self.now_ms = now_ms;
        crate::micro::tick(self);
        std::mem::take(&mut self.pending)
    }

    /// Unix seconds.
    #[must_use]
    pub fn now_unix(&self) -> i64 {
        i64::try_from(self.now_ms / 1000).unwrap_or(0)
    }

    /// Scroll state of the active channel.
    #[must_use]
    pub fn scroll_state(&self) -> ScrollState {
        self.active_channel
            .and_then(|c| self.scrolls.get(&c))
            .map_or(ScrollState::FollowTail, |s| s.state)
    }

    /// Messages of the active channel.
    #[must_use]
    pub fn active_messages(&self) -> Option<&std::collections::VecDeque<Message>> {
        self.active_channel
            .and_then(|c| self.store.messages.get(&c))
            .map(|m| &m.messages)
    }

    /// Replace composer text (after `$EDITOR`).
    pub fn set_composer_text(&mut self, s: &str) {
        let clean = crate::sanitize::sanitize_remote(s);
        self.composer.set_text(&clean);
        self.mode = Mode::Insert;
    }

    /// Whether every participant in the active voice session is DAVE-verified (§10).
    #[must_use]
    pub fn is_dave_all_verified(&self) -> bool {
        let Some(v) = &self.store.voice else {
            return false;
        };
        !v.participants.is_empty()
            && v.participants
                .iter()
                .all(|p| p.dave_verified || self.dave_verified.contains(&p.user_id))
    }

    /// Apply a backend event.
    pub fn handle_event(&mut self, ev: BackendEvent) -> Vec<Effect> {
        // Re-anchor before a delete removes the anchored message (§5).
        if let BackendEvent::MessageDeleted {
            channel_id,
            message_id,
        } = &ev
        {
            self.reanchor_on_delete(*channel_id, *message_id);
        }
        let prev_voice = self.store.voice.as_ref().map(|v| {
            let epoch = match v.dave {
                bbs_core::DaveSessionState::Active { epoch, .. } => Some(epoch),
                _ => None,
            };
            (v.channel_id, epoch)
        });
        let auto_select = matches!(ev, BackendEvent::Ready { .. }) && self.active_channel.is_none();
        let ring_before = self.store.ring.clone();
        self.store.apply(ev, self.active_channel);
        let next_voice = self.store.voice.as_ref().map(|v| {
            let epoch = match v.dave {
                bbs_core::DaveSessionState::Active { epoch, .. } => Some(epoch),
                _ => None,
            };
            (v.channel_id, epoch)
        });
        if prev_voice != next_voice {
            self.dave_verified.clear();
        }
        if auto_select {
            let first_dm = self.store.dms_by_recency().first().map(|c| c.id);
            if let Some(id) = first_dm {
                self.select_channel(id);
            }
        }
        if ring_before != self.store.ring
            && matches!(self.store.ring, CallRingState::IncomingRinging { .. })
        {
            self.pending.push(Effect::Bell);
        }
        std::mem::take(&mut self.pending)
    }

    fn reanchor_on_delete(&mut self, channel: Snowflake, id: Snowflake) {
        let Some(scroll) = self.scrolls.get(&channel).copied() else {
            return;
        };
        let ScrollState::Anchored(a) = scroll.state else {
            return;
        };
        if a.message_id != id {
            return;
        }
        let Some(msgs) = self.store.messages.get(&channel) else {
            return;
        };
        let Some(i) = msgs.index_of(id) else { return };
        let target = if i > 0 {
            msgs.messages.get(i - 1)
        } else {
            msgs.messages.get(i + 1)
        };
        let new = match target {
            Some(m) => Scroll {
                state: ScrollState::Anchored(bbs_core::ScrollAnchor::at_footer(m.id)),
                hint: None,
            },
            None => Scroll::default(),
        };
        // Footer may not exist on the target; resolution falls back to its last line.
        self.scrolls.insert(channel, new);
        if self.selected == Some(id) {
            self.selected = target.map(|m| m.id);
        }
    }

    /// Switch to a channel, emitting the select action.
    pub fn select_channel(&mut self, id: Snowflake) {
        let kind = self.store.channel(id).map(|c| c.kind);
        if kind.is_some_and(|k| k == ChannelKind::GuildCategory) {
            return;
        }
        if kind.is_some_and(ChannelKind::is_voice) {
            let guild = self.store.channel(id).and_then(|c| c.guild_id);
            self.pending.push(Effect::Action(UiAction::JoinVoice {
                guild_id: guild,
                channel_id: id,
            }));
            return;
        }
        self.active_channel = Some(id);
        self.active_guild = self.store.channel(id).and_then(|c| c.guild_id);
        self.selected = None;
        self.store.mark_read(id);
        self.pending
            .push(Effect::Action(UiAction::SelectChannel(id)));
        let entry = self.store.messages.entry(id).or_default();
        if entry.messages.is_empty() && !entry.loading {
            entry.loading = true;
        }
    }

    /// Select a guild (expands it and asks for its channels).
    pub fn select_guild(&mut self, id: Option<Snowflake>) {
        self.sidebar.expanded.insert(id);
        self.pending.push(Effect::Action(UiAction::SelectGuild(id)));
    }

    /// Paste (bracketed paste or heuristic): inserted verbatim, never sent (§9.4).
    pub fn handle_paste(&mut self, text: &str) -> Vec<Effect> {
        let clean = crate::sanitize::sanitize_remote(text).into_owned();
        if let Some(m) = self.modals.last_mut() {
            m.paste(&clean);
        } else if self.is_micro() {
            self.composer.insert_str(&clean.replace('\n', " "));
            self.micro.enter_compose();
        } else {
            self.mode = Mode::Insert;
            self.composer.insert_str(&clean);
            if self.composer.char_len() > self.cfg.max_message_len() {
                self.open(Modal::too_long(self.cfg.max_message_len()));
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Terminal resized (the anchor is never touched, I5).
    pub fn handle_resize(&mut self, cols: u16, rows: u16) -> Vec<Effect> {
        self.size = (cols, rows);
        vec![Effect::Action(UiAction::ViewportResized(
            ViewportGeometry::new(cols, rows),
        ))]
    }

    pub(crate) fn is_micro(&self) -> bool {
        self.layout.as_ref().is_some_and(|l| l.micro)
            || (self.layout.is_none()
                && (self.size.1 <= 4 || self.size.0 < 24)
                && self.size != (0, 0))
    }

    /// Handle a key press.
    pub fn handle_key(&mut self, k: KeyEvent) -> Vec<Effect> {
        if k.kind == KeyEventKind::Release {
            return Vec::new();
        }
        // 1. Recovery tier.
        if ctrl(&k, 'l') {
            self.cache.clear();
            self.pending.push(Effect::Redraw);
            return std::mem::take(&mut self.pending);
        }
        if ctrl(&k, 'z') {
            self.pending.push(Effect::Suspend);
            return std::mem::take(&mut self.pending);
        }
        if ctrl(&k, 'c') {
            self.on_ctrl_c();
            return std::mem::take(&mut self.pending);
        }
        // Ring shortcuts work everywhere except inside text inputs.
        // 2. Modal tier.
        if !self.modals.is_empty() {
            self.modal_key(k);
            return std::mem::take(&mut self.pending);
        }
        // Micro pager has its own state machine (§3).
        if self.is_micro() {
            crate::micro::handle_key(self, k);
            return std::mem::take(&mut self.pending);
        }
        if self.fkey(&k) {
            return std::mem::take(&mut self.pending);
        }
        match self.mode {
            Mode::Insert => self.insert_key(k),
            Mode::Normal => self.normal_key(k),
        }
        std::mem::take(&mut self.pending)
    }

    fn on_ctrl_c(&mut self) {
        if !self.modals.is_empty() {
            self.close_modal();
            return;
        }
        if !self.composer.is_empty() {
            self.composer.clear();
            return;
        }
        if self.now_ms.saturating_sub(self.last_ctrl_c) <= 1000 && self.last_ctrl_c != 0 {
            self.open(Modal::confirm_quit());
        } else {
            self.last_ctrl_c = self.now_ms.max(1);
        }
    }

    pub(crate) fn open(&mut self, m: Modal) {
        if let Modal::Theme(_) = m {
            self.theme_before_preview = Some(self.theme.clone());
        }
        self.modals.push(m);
    }

    /// Rescan theme sources (built-ins, user dir, btop dirs).
    pub(crate) fn rescan_themes(&mut self) {
        self.catalog = theme::ThemeCatalog::discover(&self.theme_dirs);
    }

    /// Load catalog entry `idx` and apply it (live preview).
    pub(crate) fn apply_catalog(&mut self, idx: usize) {
        match self.catalog.load(idx) {
            Ok(spec) => self.set_theme(spec),
            Err(e) => self.store.banners.push(crate::state::Banner {
                scope: "theme".to_string(),
                message: crate::sanitize::sanitize_single_line(&e.to_string()).into_owned(),
                recoverable: true,
            }),
        }
    }

    /// Theme catalog (after the last rescan).
    #[must_use]
    pub fn theme_catalog(&self) -> &theme::ThemeCatalog {
        &self.catalog
    }

    pub(crate) fn close_modal(&mut self) {
        if let Some(Modal::Theme(_)) = self.modals.last()
            && let Some(t) = self.theme_before_preview.take()
        {
            self.theme = t;
        }
        self.modals.pop();
    }

    fn modal_key(&mut self, k: KeyEvent) {
        let Some(mut m) = self.modals.pop() else {
            return;
        };
        let outcome = modals::handle_key(self, &mut m, k);
        match outcome {
            ModalOutcome::Keep => self.modals.push(m),
            ModalOutcome::Close => {
                if let Modal::Theme(_) = m
                    && let Some(t) = self.theme_before_preview.take()
                {
                    self.theme = t;
                }
            }
            ModalOutcome::CloseCommit => {
                self.cfg.theme = self.theme.name().to_string();
                self.theme_before_preview = None;
            }
            ModalOutcome::Replace(n) => {
                self.modals.push(*n);
            }
        }
    }

    /// F-keys (global in NORMAL and INSERT).
    fn fkey(&mut self, k: &KeyEvent) -> bool {
        let KeyCode::F(n) = k.code else { return false };
        match n {
            1 => self.open(Modal::help()),
            2 => {
                let m = Modal::theme(self);
                self.open(m)
            }
            3 => self.toggle_members(),
            4 => self.open(Modal::Voice),
            5 => self.open_gif(),
            6 => self.cycle_focus(1),
            7 => self.toggle_mute(),
            8 => self.toggle_deaf(),
            9 => self.answer_or_hangup(),
            10 => self.open(Modal::confirm_quit()),
            _ => return false,
        }
        true
    }

    pub(crate) fn toggle_members(&mut self) {
        let wide = self
            .layout
            .as_ref()
            .is_some_and(|l| l.h >= crate::geometry::HProfile::Wide);
        if wide {
            self.show_members = !self.show_members;
        } else {
            self.open(Modal::Members);
        }
    }

    pub(crate) fn open_gif(&mut self) {
        self.gif_req = self.gif_req.wrapping_add(1);
        let q = GifSearchQuery::default();
        self.pending.push(Effect::Action(UiAction::SearchGifs {
            request_id: self.gif_req,
            query: q,
        }));
        self.open(Modal::gif(self.gif_req));
    }

    pub(crate) fn toggle_mute(&mut self) {
        let muted = self.store.voice.as_ref().is_some_and(|v| v.self_mute);
        self.pending
            .push(Effect::Action(UiAction::SetSelfMute(!muted)));
        if let Some(v) = self.store.voice.as_mut() {
            v.self_mute = !muted;
        }
    }

    pub(crate) fn toggle_deaf(&mut self) {
        let deaf = self.store.voice.as_ref().is_some_and(|v| v.self_deaf);
        self.pending
            .push(Effect::Action(UiAction::SetSelfDeaf(!deaf)));
        if let Some(v) = self.store.voice.as_mut() {
            v.self_deaf = !deaf;
        }
    }

    pub(crate) fn answer_or_hangup(&mut self) {
        match self.store.ring.clone() {
            CallRingState::IncomingRinging { channel_id, .. } => {
                self.pending.push(Effect::Action(UiAction::JoinVoice {
                    guild_id: None,
                    channel_id,
                }));
            }
            CallRingState::OutgoingRinging { channel_id, .. } => {
                self.pending
                    .push(Effect::Action(UiAction::StopCallRing { channel_id }));
                self.pending.push(Effect::Action(UiAction::LeaveVoice));
            }
            _ => {
                if self.store.voice.is_some() {
                    self.pending.push(Effect::Action(UiAction::LeaveVoice));
                } else {
                    self.call();
                }
            }
        }
    }

    pub(crate) fn decline(&mut self) {
        match self.store.ring.clone() {
            CallRingState::IncomingRinging { channel_id, .. } => {
                self.pending
                    .push(Effect::Action(UiAction::StopCallRing { channel_id }));
                self.store.ring = CallRingState::Idle;
            }
            _ => {
                if self.store.voice.is_some() {
                    self.pending.push(Effect::Action(UiAction::LeaveVoice));
                }
            }
        }
    }

    pub(crate) fn call(&mut self) {
        if let CallRingState::IncomingRinging { .. } = self.store.ring {
            self.answer_or_hangup();
            return;
        }
        let Some(ch) = self.active_channel else {
            return;
        };
        let Some(c) = self.store.channel(ch) else {
            return;
        };
        if c.kind.is_dm() {
            self.pending.push(Effect::Action(UiAction::JoinVoice {
                guild_id: None,
                channel_id: ch,
            }));
            self.pending.push(Effect::Action(UiAction::RingCall {
                channel_id: ch,
                recipient_ids: None,
            }));
        }
    }

    pub(crate) fn join_voice(&mut self) {
        // Sidebar cursor on a voice channel, else the active channel if it can carry a call.
        if self.focus == Focus::Sidebar
            && let Some(SideItem::Channel(id, _)) =
                self.sidebar_items().get(self.sidebar.cursor).cloned()
            && let Some(c) = self.store.channel(id)
            && c.kind.is_voice()
        {
            let g = c.guild_id;
            self.pending.push(Effect::Action(UiAction::JoinVoice {
                guild_id: g,
                channel_id: id,
            }));
            return;
        }
        if let Some(ch) = self.active_channel
            && let Some(c) = self.store.channel(ch)
            && c.kind.supports_voice_call()
        {
            let g = c.guild_id;
            self.pending.push(Effect::Action(UiAction::JoinVoice {
                guild_id: g,
                channel_id: ch,
            }));
        }
    }

    pub(crate) fn screenshare(&mut self) {
        if self.broadcasting {
            self.broadcasting = false;
            self.pending.push(Effect::Action(UiAction::StopBroadcast));
            return;
        }
        if let Some(v) = &self.store.voice {
            self.broadcasting = true;
            self.pending.push(Effect::Action(UiAction::StartBroadcast {
                guild_id: v.guild_id,
                channel_id: v.channel_id,
                source: StreamSourceKind::PortalPipeWire,
                quality: StreamQuality::default(),
            }));
        }
    }

    /// Start or stop watching the first stream in the voice channel (Stage deck gate decides the sink).
    pub(crate) fn toggle_watch(&mut self) {
        if let Some(key) = self.watching.take() {
            self.pending
                .push(Effect::Action(UiAction::StopWatchingStream {
                    stream_key: key,
                }));
            return;
        }
        let Some(v) = &self.store.voice else { return };
        let Some(s) = self
            .store
            .streams
            .get(&v.channel_id)
            .and_then(|s| s.first())
        else {
            return;
        };
        let key = s.stream_key.clone();
        let sink = self.preferred_sink();
        self.watching = Some(key.clone());
        self.pending.push(Effect::Action(UiAction::WatchStream {
            stream_key: key,
            sink,
        }));
    }

    pub(crate) fn preferred_sink(&self) -> VideoSinkKind {
        if !self.cfg.prefer_inline_video {
            return VideoSinkKind::PopoutWindow;
        }
        let deck = self
            .layout
            .as_ref()
            .and_then(|l| l.decks().first().copied());
        let (w, h) = deck.map_or((0, 0), |r| (r.width, r.height));
        VideoSinkKind::InlineTerminal.resolve_effective(
            w,
            h,
            self.cfg.graphics,
            self.cfg.over_ssh,
            self.cfg.inline_video_low_tier,
            self.cfg.inline_video_ssh,
        )
    }

    /// Toggle between `PopoutWindow` (default) and `InlineTerminal` when Stage deck allows it (§6.5).
    pub(crate) fn toggle_video_sink(&mut self) {
        self.cfg.prefer_inline_video = !self.cfg.prefer_inline_video;
        if let Some(key) = self.watching.clone() {
            let sink = self.preferred_sink();
            self.pending.push(Effect::Action(UiAction::WatchStream {
                stream_key: key,
                sink,
            }));
        }
    }

    pub(crate) fn cycle_focus(&mut self, dir: i8) {
        let mut order = Vec::new();
        if let Some(l) = &self.layout {
            for p in &l.panes {
                order.push(match p.kind {
                    crate::geometry::PaneKind::Sidebar => Focus::Sidebar,
                    crate::geometry::PaneKind::Chat => Focus::Messages,
                    crate::geometry::PaneKind::Members => Focus::Members,
                    crate::geometry::PaneKind::Deck(i) => Focus::Deck(i),
                });
            }
        }
        if order.is_empty() {
            order.push(Focus::Messages);
        }
        let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = order.len();
        let next = if dir >= 0 {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.focus = order[next];
    }

    pub(crate) fn focus_pane(&mut self, n: u8) {
        if let Some(l) = &self.layout
            && let Some(p) = l.panes.get(usize::from(n.saturating_sub(1)))
        {
            self.focus = match p.kind {
                crate::geometry::PaneKind::Sidebar => Focus::Sidebar,
                crate::geometry::PaneKind::Chat => Focus::Messages,
                crate::geometry::PaneKind::Members => Focus::Members,
                crate::geometry::PaneKind::Deck(i) => Focus::Deck(i),
            };
        }
    }

    /// Jump to the next (`dir > 0`) or previous unread channel.
    pub(crate) fn jump_unread(&mut self, dir: i8) {
        let mut all: Vec<(Snowflake, u32, u32)> = self
            .store
            .dms_by_recency()
            .iter()
            .map(|c| (c.id, c.unread_count, c.mention_count))
            .collect();
        for g in self.store.ordered_guilds() {
            for c in self.store.sidebar_channels(g.id) {
                if !c.kind.is_voice() && c.kind != ChannelKind::GuildCategory {
                    all.push((c.id, c.unread_count, c.mention_count));
                }
            }
        }
        let cur = self
            .active_channel
            .and_then(|a| all.iter().position(|c| c.0 == a));
        let n = all.len();
        if n == 0 {
            return;
        }
        for step in 1..=n {
            let i = match (cur, dir >= 0) {
                (Some(c), true) => (c + step) % n,
                (Some(c), false) => (c + n - (step % n)) % n,
                (None, _) => step - 1,
            };
            if all[i].1 > 0 || all[i].2 > 0 {
                let id = all[i].0;
                self.select_channel(id);
                return;
            }
        }
    }

    /// Flattened sidebar rows.
    #[must_use]
    pub fn sidebar_items(&self) -> Vec<SideItem> {
        let mut out = vec![SideItem::Home];
        if self.sidebar.expanded.contains(&None) {
            for c in self.store.dms_by_recency() {
                out.push(SideItem::Channel(c.id, 1));
            }
        }
        for g in self.store.ordered_guilds() {
            out.push(SideItem::Guild(g.id));
            if self.sidebar.expanded.contains(&Some(g.id)) {
                let mut hidden_parent: Option<Snowflake> = None;
                for c in self.store.sidebar_channels(g.id) {
                    if c.kind == ChannelKind::GuildCategory {
                        hidden_parent = self.sidebar.collapsed.contains(&c.id).then_some(c.id);
                        out.push(SideItem::Channel(c.id, 1));
                        continue;
                    }
                    if hidden_parent.is_some() && c.parent_id == hidden_parent {
                        continue;
                    }
                    let depth = if c.kind.is_thread() {
                        3
                    } else if c.parent_id.is_some() {
                        2
                    } else {
                        1
                    };
                    out.push(SideItem::Channel(c.id, depth));
                }
            }
        }
        out
    }

    pub(crate) fn sidebar_activate(&mut self) {
        let items = self.sidebar_items();
        let Some(item) = items.get(self.sidebar.cursor).cloned() else {
            return;
        };
        match item {
            SideItem::Home => {
                if !self.sidebar.expanded.remove(&None) {
                    self.select_guild(None);
                }
            }
            SideItem::Guild(id) => {
                if !self.sidebar.expanded.remove(&Some(id)) {
                    self.select_guild(Some(id));
                }
            }
            SideItem::Channel(id, _) => {
                let is_cat = self
                    .store
                    .channel(id)
                    .is_some_and(|c| c.kind == ChannelKind::GuildCategory);
                if is_cat {
                    if !self.sidebar.collapsed.remove(&id) {
                        self.sidebar.collapsed.insert(id);
                    }
                } else {
                    self.select_channel(id);
                    if self.store.channel(id).is_some_and(|c| !c.kind.is_voice()) {
                        self.focus = Focus::Messages;
                    }
                }
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(msgs) = self.active_messages() else {
            return;
        };
        if msgs.is_empty() {
            return;
        }
        let n = msgs.len() as isize;
        let cur = self
            .selected
            .and_then(|s| msgs.iter().position(|m| m.id == s))
            .map_or(n, |i| i as isize);
        let next = (cur + delta).clamp(0, n);
        if next >= n {
            self.selected = None;
            if let Some(ch) = self.active_channel {
                self.scrolls.insert(ch, Scroll::default());
            }
            return;
        }
        let id = msgs[next as usize].id;
        self.selected = Some(id);
        self.reveal_selected();
    }

    pub(crate) fn reveal_selected(&mut self) {
        let (Some(id), Some(ch)) = (self.selected, self.active_channel) else {
            return;
        };
        let Some(view) = crate::render::messages_view_params(self) else {
            return;
        };
        let (params, pkey, height) = view;
        let store = &self.store;
        let Some(msgs) = store.messages.get(&ch) else {
            return;
        };
        let res = |k, i| crate::render::resolve_mention(store, k, i);
        let ment = |m: &Message| store.mentions_me(m);
        let tl = crate::scroll::Timeline {
            msgs: &msgs.messages,
            params,
            pkey,
            res: &res,
            mentions: &ment,
            playback: store.playback.as_ref(),
        };
        let cur = self.scrolls.get(&ch).copied().unwrap_or_default();
        let v = tl.view(&mut self.cache, &cur, height);
        if v.lines.iter().any(|l| l.message_id == id) {
            return;
        }
        let s = tl.reveal(&mut self.cache, id, height);
        self.scrolls.insert(ch, s);
    }

    /// Scroll the active channel by `delta` lines.
    pub fn scroll_lines(&mut self, delta: isize) {
        let Some(ch) = self.active_channel else {
            return;
        };
        let Some((params, pkey, height)) = crate::render::messages_view_params(self) else {
            return;
        };
        let store = &self.store;
        let Some(msgs) = store.messages.get(&ch) else {
            return;
        };
        if msgs.messages.is_empty() {
            return;
        }
        let res = |k, i| crate::render::resolve_mention(store, k, i);
        let ment = |m: &Message| store.mentions_me(m);
        let tl = crate::scroll::Timeline {
            msgs: &msgs.messages,
            params,
            pkey,
            res: &res,
            mentions: &ment,
            playback: store.playback.as_ref(),
        };
        let cur = self.scrolls.get(&ch).copied().unwrap_or_default();
        let s = tl.scroll_by(&mut self.cache, &cur, delta, height);
        self.scrolls.insert(ch, s);
    }

    pub(crate) fn request_older(&mut self) {
        let Some(ch) = self.active_channel else {
            return;
        };
        let Some(entry) = self.store.messages.get_mut(&ch) else {
            return;
        };
        if entry.loading || !entry.has_more_before {
            return;
        }
        let Some(first) = entry.messages.front().map(|m| m.id) else {
            return;
        };
        entry.loading = true;
        self.pending
            .push(Effect::Action(UiAction::LoadOlderMessages {
                channel_id: ch,
                before: first,
                limit: 50,
            }));
    }

    pub(crate) fn selected_message(&self) -> Option<&Message> {
        let id = self.selected?;
        self.active_messages()?.iter().find(|m| m.id == id)
    }

    pub(crate) fn reply_to_selected(&mut self) {
        let msg = self
            .selected_message()
            .or_else(|| self.active_messages().and_then(|m| m.back()))
            .cloned();
        if let Some(m) = msg {
            let snippet = crate::sanitize::sanitize_single_line(&m.content);
            self.composer.target = ComposeTarget::Reply {
                message_id: m.id,
                author: m.author.display_name().to_string(),
                snippet: crate::text::truncate(&snippet, 40, "…"),
            };
            self.mode = Mode::Insert;
        }
    }

    fn edit_last_own(&mut self) {
        let me = self.store.me.as_ref().map(|u| u.id);
        let msg = self
            .active_messages()
            .and_then(|ms| ms.iter().rev().find(|m| Some(m.author.id) == me))
            .cloned();
        if let Some(m) = msg {
            self.composer.set_text(&m.content);
            self.composer.target = ComposeTarget::Edit { message_id: m.id };
        }
    }

    pub(crate) fn send(&mut self) {
        let Some(ch) = self.active_channel else {
            return;
        };
        if self.composer.is_empty() && self.composer.attachments.is_empty() {
            return;
        }
        if self.composer.char_len() > self.cfg.max_message_len() {
            self.open(Modal::too_long(self.cfg.max_message_len()));
            return;
        }
        let target = self.composer.target.clone();
        let attachments = std::mem::take(&mut self.composer.attachments);
        let content = self.composer.take();
        self.composer.target = ComposeTarget::New;
        match target {
            ComposeTarget::Edit { message_id } => {
                self.pending.push(Effect::Action(UiAction::EditMessage {
                    channel_id: ch,
                    message_id,
                    content,
                }));
            }
            ComposeTarget::Reply { message_id, .. } => {
                self.pending.push(Effect::Action(UiAction::SendMessage {
                    channel_id: ch,
                    content,
                    reply_to: Some(message_id),
                    mention_replied_user: true,
                    attachment_paths: attachments,
                }));
            }
            ComposeTarget::New => {
                self.pending.push(Effect::Action(UiAction::SendMessage {
                    channel_id: ch,
                    content,
                    reply_to: None,
                    mention_replied_user: true,
                    attachment_paths: attachments,
                }));
            }
        }
        if let Some(ch) = self.active_channel {
            self.scrolls.insert(ch, Scroll::default());
        }
        self.selected = None;
    }

    /// Split oversized composer text into ≤limit chunks on whitespace and send each.
    pub(crate) fn send_split(&mut self) {
        let Some(ch) = self.active_channel else {
            return;
        };
        let limit = self.cfg.max_message_len();
        let text = self.composer.take();
        let reply = match &self.composer.target {
            ComposeTarget::Reply { message_id, .. } => Some(*message_id),
            _ => None,
        };
        self.composer.target = ComposeTarget::New;
        let mut chunk = String::new();
        let mut first = true;
        for word in text.split_inclusive(char::is_whitespace) {
            if chunk.chars().count() + word.chars().count() > limit && !chunk.is_empty() {
                self.pending.push(Effect::Action(UiAction::SendMessage {
                    channel_id: ch,
                    content: std::mem::take(&mut chunk),
                    reply_to: if first { reply } else { None },
                    mention_replied_user: true,
                    attachment_paths: Vec::new(),
                }));
                first = false;
            }
            if word.chars().count() > limit {
                for c in word.chars() {
                    if chunk.chars().count() >= limit {
                        self.pending.push(Effect::Action(UiAction::SendMessage {
                            channel_id: ch,
                            content: std::mem::take(&mut chunk),
                            reply_to: if first { reply } else { None },
                            mention_replied_user: true,
                            attachment_paths: Vec::new(),
                        }));
                        first = false;
                    }
                    chunk.push(c);
                }
            } else {
                chunk.push_str(word);
            }
        }
        if !chunk.trim().is_empty() {
            self.pending.push(Effect::Action(UiAction::SendMessage {
                channel_id: ch,
                content: chunk,
                reply_to: if first { reply } else { None },
                mention_replied_user: true,
                attachment_paths: Vec::new(),
            }));
        }
    }

    pub(crate) fn send_as_file(&mut self) {
        let Some(ch) = self.active_channel else {
            return;
        };
        let reply = match &self.composer.target {
            ComposeTarget::Reply { message_id, .. } => Some(*message_id),
            _ => None,
        };
        let content = self.composer.take();
        self.composer.target = ComposeTarget::New;
        self.pending.push(Effect::SendAsFile {
            channel_id: ch,
            content,
            reply_to: reply,
        });
    }

    fn insert_key(&mut self, k: KeyEvent) {
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let ctl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.ctrl_x {
            self.ctrl_x = false;
            if ctrl(&k, 'e') {
                self.pending
                    .push(Effect::ExternalEditor(self.composer.text().to_string()));
                return;
            }
        }
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.composer.insert_char('\n')
            }
            KeyCode::Enter => self.send(),
            KeyCode::Char('j') if ctl => self.composer.insert_char('\n'),
            KeyCode::Tab => self.autocomplete(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Char('h') if ctl => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete(),
            KeyCode::Char('d') if ctl => self.composer.delete(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Char('b') if ctl => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Char('f') if ctl => self.composer.right(),
            KeyCode::Home => self.composer.home(),
            KeyCode::Char('a') if ctl => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::Char('e') if ctl => self.composer.end(),
            KeyCode::Char('u') if ctl => self.composer.kill_to_bol(),
            KeyCode::Char('w') if ctl => self.composer.kill_word_back(),
            KeyCode::Char('y') if ctl => self.composer.yank(),
            KeyCode::Char('t') if ctl => self.composer.transpose(),
            KeyCode::Char('x') if ctl => self.ctrl_x = true,
            KeyCode::Char('k') if ctl => match self.cfg.ctrl_k {
                CtrlKMode::Kill => {
                    self.composer.kill_to_eol();
                }
                CtrlKMode::Switcher => self.open(Modal::switcher(self)),
                CtrlKMode::Dwim => {
                    if self.composer.is_empty() || self.composer.at_end() {
                        self.open(Modal::switcher(self));
                    } else {
                        self.composer.kill_to_eol();
                    }
                }
            },
            KeyCode::Char('g') if ctl => self.open_gif(),
            KeyCode::Char('o') if ctl => self.toggle_mute(),
            KeyCode::Char('n') if ctl => self.jump_unread(1),
            KeyCode::Char('p') if ctl => self.jump_unread(-1),
            KeyCode::Char('r') if ctl => self.reply_to_selected(),
            KeyCode::Char('b') if alt => self.composer.word_left(),
            KeyCode::Char('f') if alt => self.composer.word_right(),
            KeyCode::Char('d') if alt => self.composer.kill_word_fwd(),
            KeyCode::Up if self.composer.is_empty() => self.edit_last_own(),
            KeyCode::PageUp => self.scroll_page(-1),
            KeyCode::PageDown => self.scroll_page(1),
            KeyCode::Char(c) if !ctl && !alt => self.composer.insert_char(c),
            _ => {}
        }
    }

    fn scroll_page(&mut self, dir: isize) {
        let h = self
            .layout
            .as_ref()
            .map_or(10, |l| l.messages.height.max(2));
        self.scroll_lines(dir * isize::try_from(h.saturating_sub(1).max(1)).unwrap_or(1));
    }

    fn autocomplete(&mut self) {
        let text = self.composer.text()[..self.composer.cursor()].to_string();
        let Some(at) = text.rfind('@') else { return };
        let prefix = &text[at + 1..];
        if prefix.contains(char::is_whitespace) {
            return;
        }
        let lower = prefix.to_lowercase();
        let mut names: Vec<String> = self
            .store
            .users
            .values()
            .map(|u| u.username.clone())
            .filter(|n| n.to_lowercase().starts_with(&lower))
            .collect();
        names.sort();
        if let Some(n) = names.first() {
            // Char-based skip: case folding can change byte lengths (e.g. `İ`).
            let rest: String = n.chars().skip(prefix.chars().count()).collect();
            self.composer.insert_str(&format!("{rest} "));
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        if ctrl(&k, 'k') {
            self.open(Modal::switcher(self));
            return;
        }
        if ctrl(&k, 't') {
            {
                let m = Modal::theme(self);
                self.open(m)
            };
            return;
        }
        if ctrl(&k, 'g') {
            self.open_gif();
            return;
        }
        if ctrl(&k, 'o') {
            self.toggle_mute();
            return;
        }
        if ctrl(&k, 'n') {
            self.jump_unread(1);
            return;
        }
        if ctrl(&k, 'p') {
            self.jump_unread(-1);
            return;
        }
        if ctrl(&k, 'q') {
            self.open(Modal::confirm_quit());
            return;
        }
        if ctrl(&k, 'r') {
            self.reply_to_selected();
            return;
        }
        match k.code {
            KeyCode::Tab => return self.cycle_focus(1),
            KeyCode::BackTab => return self.cycle_focus(-1),
            KeyCode::Up => return self.nav(-1),
            KeyCode::Down => return self.nav(1),
            KeyCode::PageUp => return self.scroll_page(-1),
            KeyCode::PageDown => return self.scroll_page(1),
            KeyCode::Home => {
                if let Some(ch) = self.active_channel {
                    if let Some((params, pkey, height)) = crate::render::messages_view_params(self)
                    {
                        let store = &self.store;
                        if let Some(msgs) = store.messages.get(&ch)
                            && !msgs.messages.is_empty()
                        {
                            let res = |kk, i| crate::render::resolve_mention(store, kk, i);
                            let ment = |m: &Message| store.mentions_me(m);
                            let tl = crate::scroll::Timeline {
                                msgs: &msgs.messages,
                                params,
                                pkey,
                                res: &res,
                                mentions: &ment,
                                playback: store.playback.as_ref(),
                            };
                            let s = tl.top_scroll(&mut self.cache, height);
                            self.scrolls.insert(ch, s);
                        }
                    }
                    self.request_older();
                }
                return;
            }
            KeyCode::End => {
                if let Some(ch) = self.active_channel {
                    self.scrolls.insert(ch, Scroll::default());
                }
                self.selected = None;
                return;
            }
            KeyCode::Enter => return self.normal_enter(),
            KeyCode::Esc => {
                self.selected = None;
                return;
            }
            KeyCode::Delete => {
                if let Some(m) = self.selected_message()
                    && self
                        .store
                        .me
                        .as_ref()
                        .is_some_and(|me| me.id == m.author.id)
                {
                    let id = m.id;
                    self.open(Modal::confirm_delete(id));
                }
                return;
            }
            KeyCode::Left if self.focus == Focus::Sidebar => {
                self.sidebar_collapse();
                return;
            }
            KeyCode::Right if self.focus == Focus::Sidebar => {
                self.sidebar_activate();
                return;
            }
            _ => {}
        }
        let Some(c) = plain(&k) else { return };
        let in_voice_ctx = matches!(self.focus, Focus::Deck(_)) && self.deck_is_stage();
        match c {
            'k' => self.open(Modal::switcher(self)),
            'i' => self.mode = Mode::Insert,
            'r' => self.reply_to_selected(),
            'e' => {
                let target = self
                    .selected
                    .or_else(|| self.active_messages().and_then(|m| m.back()).map(|m| m.id));
                if let Some(t) = target {
                    self.open(Modal::react(t));
                }
            }
            'E' => {
                if let Some(m) = self.selected_message().cloned()
                    && self
                        .store
                        .me
                        .as_ref()
                        .is_some_and(|me| me.id == m.author.id)
                {
                    self.composer.set_text(&m.content);
                    self.composer.target = ComposeTarget::Edit { message_id: m.id };
                    self.mode = Mode::Insert;
                }
            }
            't' => self.open(Modal::threads(self)),
            'p' => self.open(Modal::pins(self)),
            '/' => self.open(Modal::search()),
            'T' => {
                let m = Modal::theme(self);
                self.open(m)
            }
            'G' => self.open_gif(),
            ']' => self.jump_unread(1),
            '[' => self.jump_unread(-1),
            'm' => self.toggle_mute(),
            'd' => self.toggle_deaf(),
            'v' if in_voice_ctx => self.open(Modal::Dave),
            'v' => self.join_voice(),
            'a' => self.call(),
            'h' => self.decline(),
            's' => self.screenshare(),
            'w' => self.toggle_watch(),
            'W' => self.toggle_video_sink(),
            'M' => self.toggle_members(),
            'V' => self.open(Modal::Voice),
            'D' => self.open(Modal::Dave),
            'c' | 'g' => self.focus = Focus::Sidebar,
            '?' => self.open(Modal::help()),
            'q' => self.open(Modal::confirm_quit()),
            'x' => {
                if in_voice_ctx {
                    self.volume_step(0, true);
                } else if let Some(id) = self.selected
                    && !self.revealed.remove(&id)
                {
                    self.revealed.insert(id);
                }
            }
            '+' | '=' if in_voice_ctx => self.volume_step(10, false),
            '-' if in_voice_ctx => self.volume_step(-10, false),
            '0' if in_voice_ctx => self.volume_reset(),
            'n' | '+' | '-' | 'R' | 'L' => self.gdm_key(c),
            'o' => self.play_selected_voice(),
            '1'..='6' => self.focus_pane(u8::try_from(c as u32 - '0' as u32).unwrap_or(1)),
            _ => {}
        }
    }

    fn deck_is_stage(&self) -> bool {
        let Focus::Deck(i) = self.focus else {
            return false;
        };
        crate::panes::deck_kind(self, i) == crate::panes::DeckKind::Stage
    }

    fn gdm_key(&mut self, c: char) {
        let ch = if self.focus == Focus::Sidebar {
            match self.sidebar_items().get(self.sidebar.cursor) {
                Some(SideItem::Channel(id, _)) => Some(*id),
                Some(SideItem::Home) => None,
                _ => return,
            }
        } else {
            self.active_channel
        };
        let is_gdm = ch
            .and_then(|id| self.store.channel(id))
            .is_some_and(|c| c.kind.is_group_dm());
        let in_dms = self.focus == Focus::Sidebar
            && (ch.is_none()
                || ch
                    .and_then(|id| self.store.channel(id))
                    .is_some_and(|c| c.kind.is_dm()));
        match c {
            'n' if in_dms
                || is_gdm
                || ch
                    .and_then(|id| self.store.channel(id))
                    .is_some_and(|c| c.kind.is_dm()) =>
            {
                self.open(Modal::gdm_new(self));
            }
            '+' if is_gdm => {
                if let Some(id) = ch {
                    self.open(Modal::gdm_add(self, id));
                }
            }
            '-' if is_gdm => {
                if let Some(id) = ch {
                    self.open(Modal::gdm_remove(self, id));
                }
            }
            'R' if is_gdm => {
                if let Some(id) = ch {
                    self.open(Modal::gdm_rename(id));
                }
            }
            'L' if is_gdm => {
                if let Some(id) = ch {
                    self.open(Modal::confirm_leave(id));
                }
            }
            _ => {}
        }
    }

    pub(crate) fn volume_step(&mut self, delta: i16, toggle_mute: bool) {
        let Some(v) = self.store.voice.as_mut() else {
            return;
        };
        let idx = self.deck_cursor.min(v.participants.len().saturating_sub(1));
        let Some(p) = v.participants.get_mut(idx) else {
            return;
        };
        let new = if toggle_mute {
            if p.volume == 0 { 100 } else { 0 }
        } else {
            u8::try_from((i16::from(p.volume) + delta).clamp(0, 200)).unwrap_or(100)
        };
        p.set_volume(new);
        let uid = p.user_id;
        self.pending
            .push(Effect::Action(UiAction::SetParticipantVolume {
                user_id: uid,
                volume: new,
            }));
    }

    pub(crate) fn volume_reset(&mut self) {
        let Some(v) = self.store.voice.as_mut() else {
            return;
        };
        let idx = self.deck_cursor.min(v.participants.len().saturating_sub(1));
        let Some(p) = v.participants.get_mut(idx) else {
            return;
        };
        p.set_volume(100);
        let uid = p.user_id;
        self.pending
            .push(Effect::Action(UiAction::SetParticipantVolume {
                user_id: uid,
                volume: 100,
            }));
    }

    pub(crate) fn play_selected_voice(&mut self) {
        let Some(m) = self
            .selected_message()
            .or_else(|| {
                self.active_messages()
                    .and_then(|ms| ms.iter().rev().find(|m| m.is_voice_message()))
            })
            .cloned()
        else {
            return;
        };
        if let Some(a) = m.attachments.iter().find(|a| a.is_voice_message()) {
            let playing = self
                .store
                .playback
                .as_ref()
                .is_some_and(|p| p.playing && p.attachment_id == a.id);
            if playing {
                self.pending
                    .push(Effect::Action(UiAction::StopVoiceMessage));
            } else {
                self.pending
                    .push(Effect::Action(UiAction::PlayVoiceMessage {
                        message_id: m.id,
                        attachment: a.clone(),
                    }));
            }
        }
    }

    fn sidebar_collapse(&mut self) {
        let items = self.sidebar_items();
        match items.get(self.sidebar.cursor) {
            Some(SideItem::Home) => {
                self.sidebar.expanded.remove(&None);
            }
            Some(SideItem::Guild(id)) => {
                self.sidebar.expanded.remove(&Some(*id));
            }
            Some(SideItem::Channel(id, _)) => {
                // Jump to the owning guild row.
                let gid = self.store.channel(*id).and_then(|c| c.guild_id);
                if let Some(pos) = items.iter().position(|i| match (i, gid) {
                    (SideItem::Guild(g), Some(x)) => *g == x,
                    (SideItem::Home, None) => true,
                    _ => false,
                }) {
                    self.sidebar.cursor = pos;
                }
            }
            None => {}
        }
    }

    fn nav(&mut self, dir: isize) {
        match self.focus {
            Focus::Sidebar => {
                let n = self.sidebar_items().len();
                if n > 0 {
                    self.sidebar.cursor =
                        (self.sidebar.cursor as isize + dir).clamp(0, n as isize - 1) as usize;
                }
            }
            Focus::Messages => {
                self.move_selection(dir);
                if dir < 0 {
                    let first = self.active_messages().and_then(|m| m.front()).map(|m| m.id);
                    if self.selected.is_some() && self.selected == first {
                        self.request_older();
                    }
                }
            }
            Focus::Members => {
                self.members_cursor = (self.members_cursor as isize + dir).max(0) as usize;
            }
            Focus::Deck(_) => {
                self.deck_cursor = (self.deck_cursor as isize + dir).max(0) as usize;
            }
        }
    }

    fn normal_enter(&mut self) {
        match self.focus {
            Focus::Sidebar => self.sidebar_activate(),
            Focus::Messages => {
                if self
                    .selected_message()
                    .is_some_and(Message::is_voice_message)
                {
                    self.play_selected_voice();
                } else {
                    self.mode = Mode::Insert;
                }
            }
            Focus::Members | Focus::Deck(_) => self.mode = Mode::Insert,
        }
    }
}
