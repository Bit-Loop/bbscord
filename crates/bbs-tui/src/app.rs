//! The application model: store + UI state, event application, and the 4-tier input router (§9).

use std::collections::{HashMap, HashSet};

use bbs_core::{
    BackendEvent, CallRingState, ChannelKind, GifSearchQuery, Message, RgbaFrame, ScrollState,
    Snowflake, StreamQuality, StreamSourceKind, UiAction, VideoSinkKind, ViewportGeometry,
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::charset::Glyphs;
use crate::composer::{AutocompleteState, ComposeTarget, Composer};
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
    /// Request clipboard payload (image, file URIs, or text) from the host runner (`Ctrl+V`).
    PasteClipboard,
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
    /// A collapsible guild folder (index into `Store::folders`).
    Folder(usize),
    /// A guild `(guild_id, indent_depth)`.
    Guild(Snowflake, u8),
    /// A channel `(channel_id, indent_depth)`.
    Channel(Snowflake, u8),
    /// A voice channel occupant `(channel_id, user_id, indent_depth)`.
    VoiceMember(Snowflake, Snowflake, u8),
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
    /// Collapsed guild folders (index into `Store::folders`).
    pub collapsed_folders: HashSet<usize>,
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
    /// Composer autocomplete popup state (`@mention`, `:emoji:`, `#channel`).
    pub autocomplete: AutocompleteState,
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
    pub(crate) gif_grid_cols: usize,
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
    // Per-frame hit-test caches populated during rendering for mouse handling.
    pub(crate) sidebar_rows: Vec<(u16, SideItem)>,
    pub(crate) visible_message_rows: Vec<(u16, Snowflake)>,
    pub(crate) member_rows: Vec<(u16, Snowflake)>,
    pub(crate) deck_rows: Vec<(u16, usize)>,
    pub(crate) modal_rect: Option<Rect>,
    pub(crate) modal_rows: Vec<(Rect, usize)>,
    pub(crate) confirm_buttons: Option<(Rect, Rect)>,
    pub(crate) autocomplete_rect: Option<Rect>,
    pub(crate) autocomplete_rows: Vec<(Rect, usize)>,
    pub(crate) last_click: Option<(u64, u16, u16)>,
    pub(crate) last_acked: HashMap<Snowflake, Snowflake>,
    pub(crate) last_typing_sent_ms: u64,
    pub(crate) pending_open_url: Option<String>,
    pub(crate) pending_copy_text: Option<String>,
    pub(crate) last_attach_dir: Option<std::path::PathBuf>,
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
            focus: Focus::Sidebar,
            composer: Composer::default(),
            autocomplete: AutocompleteState::default(),
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
            gif_grid_cols: 1,
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
            sidebar_rows: Vec::new(),
            visible_message_rows: Vec::new(),
            member_rows: Vec::new(),
            deck_rows: Vec::new(),
            modal_rect: None,
            modal_rows: Vec::new(),
            confirm_buttons: None,
            autocomplete_rect: None,
            autocomplete_rows: Vec::new(),
            last_click: None,
            last_acked: HashMap::new(),
            last_typing_sent_ms: 0,
            pending_open_url: None,
            pending_copy_text: None,
            last_attach_dir: None,
            cfg,
        }
    }

    /// Maximum attachments Discord permits on a single message (`10`).
    pub const MAX_ATTACHMENTS: usize = 10;

    /// Stage a local file/image path for upload with the next message.
    ///
    /// Returns `true` if newly added, `false` if already staged or at the 10-file limit.
    pub fn stage_attachment(&mut self, path: String) -> bool {
        if self.composer.attachments.iter().any(|p| p == &path) {
            return false;
        }
        if self.composer.attachments.len() >= Self::MAX_ATTACHMENTS {
            self.store.banners.push(crate::state::Banner {
                scope: "attach".to_string(),
                message: format!("Discord allows at most {} attachments per message.", Self::MAX_ATTACHMENTS),
                recoverable: true,
            });
            return false;
        }
        self.composer.attachments.push(path);
        true
    }

    /// Toggle a local file/image path in the staged attachments list (for multi-select).
    pub fn toggle_staged_attachment(&mut self, path: String) -> bool {
        if let Some(pos) = self.composer.attachments.iter().position(|p| p == &path) {
            self.composer.attachments.remove(pos);
            false
        } else {
            self.stage_attachment(path)
        }
    }

    /// Remove the most recently staged attachment (`Alt+X`).
    pub fn pop_staged_attachment(&mut self) -> Option<String> {
        self.composer.attachments.pop()
    }

    /// Open the interactive file & image attachment browser (`Alt+A` / `A` / `/attach`).
    pub fn open_attach(&mut self) {
        let dir = self.last_attach_dir.clone();
        self.open(Modal::attach(dir));
    }

    /// Take a pending URL/attachment queued by the user (`o` or double-click) for the host runner
    /// to open in the system browser / opener.
    pub fn take_open_url(&mut self) -> Option<String> {
        self.pending_open_url.take()
    }

    /// Take pending text queued by the user (`y` copy message / `u` copy URL) for the host runner
    /// to place on the system clipboard (e.g., OSC 52 or wl-copy/xclip).
    pub fn take_copy_text(&mut self) -> Option<String> {
        self.pending_copy_text.take()
    }

    /// Returns `true` when any modal overlay (picker, GIF search, help, confirm, etc.) is active.
    #[must_use]
    pub fn has_modal(&self) -> bool {
        !self.modals.is_empty()
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

    /// Advance the clock (unix ms). Drives typing expiry, debounced GIF search, auto-read and ring timeouts.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect> {
        self.now_ms = now_ms;
        if let Some(Modal::Gif(g)) = self.modals.last()
            && g.pending_search_at_ms.is_some_and(|due| now_ms >= due)
            && let Some(Modal::Gif(mut g)) = self.modals.pop()
        {
            modals::dispatch_gif_search(self, &mut g);
            self.modals.push(Modal::Gif(g));
        }
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
        let gif_done_req = match &ev {
            BackendEvent::GifSearchResults { request_id, .. }
            | BackendEvent::GifSearchFailed { request_id, .. } => Some(*request_id),
            _ => None,
        };
        let ack_candidate = match &ev {
            BackendEvent::MessagesLoaded { channel_id, .. } => Some(*channel_id),
            BackendEvent::MessageCreated(m) => Some(m.channel_id),
            _ => None,
        };
        let ring_before = self.store.ring.clone();
        self.store.apply(ev, self.active_channel);
        if let Some(req_id) = gif_done_req
            && let Some(Modal::Gif(g)) = self.modals.last_mut()
            && g.request_id == req_id
        {
            g.loading = false;
        }
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
        if let Some(ch) = ack_candidate
            && self.active_channel == Some(ch)
            && self.scroll_state() == ScrollState::FollowTail
        {
            self.maybe_ack_channel(ch);
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

    /// Switch to a channel, emitting the select action (or toggling voice join/leave).
    pub fn select_channel(&mut self, id: Snowflake) {
        let kind = self.store.channel(id).map(|c| c.kind);
        if kind.is_some_and(|k| k == ChannelKind::GuildCategory) {
            return;
        }
        if kind.is_some_and(ChannelKind::is_voice) {
            if self.store.voice.as_ref().is_some_and(|v| v.channel_id == id) {
                self.pending.push(Effect::Action(UiAction::LeaveVoice));
            } else {
                let guild = self.store.channel(id).and_then(|c| c.guild_id);
                self.pending.push(Effect::Action(UiAction::JoinVoice {
                    guild_id: guild,
                    channel_id: id,
                }));
            }
            return;
        }
        self.active_channel = Some(id);
        self.active_guild = self.store.channel(id).and_then(|c| c.guild_id);
        self.selected = None;
        self.autocomplete.clear();
        self.maybe_ack_channel(id);
        self.store.mark_read(id);
        self.pending
            .push(Effect::Action(UiAction::SelectChannel(id)));
        let entry = self.store.messages.entry(id).or_default();
        if entry.messages.is_empty() && !entry.loading {
            entry.loading = true;
        }
    }

    /// Open a channel interactively (sidebar Enter/click or Quick Switcher): selects it and
    /// enters composer Insert mode when `cfg.auto_focus` is enabled.
    pub(crate) fn open_channel(&mut self, id: Snowflake) {
        let is_voice_or_cat = self
            .store
            .channel(id)
            .is_some_and(|c| c.kind.is_voice() || c.kind == ChannelKind::GuildCategory);
        self.select_channel(id);
        if !is_voice_or_cat {
            self.focus = Focus::Messages;
            if self.cfg.auto_focus && !self.is_micro() {
                self.mode = Mode::Insert;
            }
        }
    }

    /// Emit a deduplicated [`UiAction::AckMessage`] if `ch` has unacknowledged messages.
    pub(crate) fn maybe_ack_channel(&mut self, ch: Snowflake) {
        let latest = self
            .store
            .channel(ch)
            .and_then(|c| c.last_message_id)
            .into_iter()
            .chain(
                self.store
                    .messages
                    .get(&ch)
                    .and_then(|m| m.messages.back().map(|msg| msg.id)),
            )
            .max_by_key(|id| id.as_u64());
        let Some(message_id) = latest else {
            return;
        };
        let needs_ack = self.store.channel(ch).is_some_and(|c| {
            c.unread_count > 0
                || c.mention_count > 0
                || c.last_ack_message_id
                    .is_some_and(|ack| message_id.as_u64() > ack.as_u64())
        });
        if !needs_ack {
            return;
        }
        if self.last_acked.get(&ch) == Some(&message_id) {
            return;
        }
        self.last_acked.insert(ch, message_id);
        self.store.mark_read(ch);
        self.pending.push(Effect::Action(UiAction::AckMessage {
            channel_id: ch,
            message_id,
        }));
    }

    /// Select a guild (expands it and asks for its channels).
    pub fn select_guild(&mut self, id: Option<Snowflake>) {
        self.sidebar.expanded.insert(id);
        self.pending.push(Effect::Action(UiAction::SelectGuild(id)));
    }

    /// Handle a terminal mouse event (clicks, double-clicks, and wheel scrolling).
    pub fn handle_mouse(&mut self, ev: MouseEvent) -> Vec<Effect> {
        let (x, y) = (ev.column, ev.row);
        let point_in = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();

        // 1. Modal tier: clicks inside activate rows/buttons; clicks outside dismiss; wheel scrolls.
        if !self.modals.is_empty() {
            match ev.kind {
                MouseEventKind::ScrollUp => {
                    if let Some(mut m) = self.modals.pop() {
                        modals::scroll_modal(self, &mut m, -3);
                        self.modals.push(m);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if let Some(mut m) = self.modals.pop() {
                        modals::scroll_modal(self, &mut m, 3);
                        self.modals.push(m);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some((no_rect, yes_rect)) = self.confirm_buttons {
                        if point_in(no_rect) {
                            self.close_modal();
                            return std::mem::take(&mut self.pending);
                        }
                        if point_in(yes_rect) {
                            if let Some(Modal::Confirm { kind, .. }) = self.modals.pop() {
                                let _ = modals::confirm_choice(self, kind, true);
                            }
                            return std::mem::take(&mut self.pending);
                        }
                    }
                    if let Some(&(_, idx)) = self.modal_rows.iter().find(|(r, _)| point_in(*r)) {
                        if let Some(mut m) = self.modals.pop() {
                            let outcome = modals::activate_row(self, &mut m, idx);
                            self.apply_modal_outcome(m, outcome);
                        }
                        return std::mem::take(&mut self.pending);
                    }
                    if self.modal_rect.is_some_and(|mr| !point_in(mr)) {
                        self.close_modal();
                    }
                }
                _ => {}
            }
            return std::mem::take(&mut self.pending);
        }

        if self.is_micro() {
            return Vec::new();
        }

        // 2. Autocomplete popup overlay in Insert mode.
        if self.mode == Mode::Insert
            && self.autocomplete.is_open()
            && let Some(ar) = self.autocomplete_rect
        {
            match ev.kind {
                MouseEventKind::ScrollUp if point_in(ar) => {
                    self.autocomplete.move_up();
                    return std::mem::take(&mut self.pending);
                }
                MouseEventKind::ScrollDown if point_in(ar) => {
                    self.autocomplete.move_down();
                    return std::mem::take(&mut self.pending);
                }
                MouseEventKind::Down(MouseButton::Left) if point_in(ar) => {
                    if let Some(&(_, idx)) =
                        self.autocomplete_rows.iter().find(|(r, _)| point_in(*r))
                    {
                        self.autocomplete.accept_index(idx, &mut self.composer);
                    }
                    return std::mem::take(&mut self.pending);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.autocomplete.clear();
                }
                _ => {}
            }
        }

        let Some(layout) = self.layout.clone() else {
            return std::mem::take(&mut self.pending);
        };

        // 3. Wheel scroll over hovered pane.
        match ev.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let down = matches!(ev.kind, MouseEventKind::ScrollDown);
                let delta: isize = if down { 3 } else { -3 };
                if point_in(layout.messages) {
                    self.scroll_lines(delta);
                } else {
                    for p in &layout.panes {
                        if !point_in(p.rect) {
                            continue;
                        }
                        match p.kind {
                            crate::geometry::PaneKind::Sidebar => {
                                let n = self.sidebar_items().len();
                                if n > 0 {
                                    self.sidebar.cursor = (self.sidebar.cursor as isize + delta)
                                        .clamp(0, n as isize - 1)
                                        as usize;
                                }
                            }
                            crate::geometry::PaneKind::Members => {
                                self.members_cursor =
                                    (self.members_cursor as isize + delta).max(0) as usize;
                            }
                            crate::geometry::PaneKind::Deck(_) => {
                                self.deck_cursor =
                                    (self.deck_cursor as isize + delta).max(0) as usize;
                            }
                            crate::geometry::PaneKind::Chat => {
                                self.scroll_lines(delta);
                            }
                        }
                        break;
                    }
                }
                return std::mem::take(&mut self.pending);
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return std::mem::take(&mut self.pending),
        }

        // 4. Left-click: focus pane and activate hit-tested rows/items.
        let is_double = self.last_click.is_some_and(|(t, lx, ly)| {
            lx == x && ly == y && self.now_ms >= t && self.now_ms.saturating_sub(t) <= 500
        });
        self.last_click = Some((self.now_ms, x, y));

        if point_in(layout.composer) {
            self.focus = Focus::Messages;
            self.mode = Mode::Insert;
            return std::mem::take(&mut self.pending);
        }

        if point_in(layout.messages) {
            self.focus = Focus::Messages;
            self.mode = Mode::Normal;
            if let Some(&(_, msg_id)) = self.visible_message_rows.iter().find(|(ry, _)| *ry == y) {
                self.selected = Some(msg_id);
                if is_double {
                    if self
                        .selected_message()
                        .is_some_and(Message::is_voice_message)
                    {
                        self.play_selected_voice();
                    } else {
                        self.open_selected_media();
                    }
                }
            }
            return std::mem::take(&mut self.pending);
        }

        for p in &layout.panes {
            if !point_in(p.rect) {
                continue;
            }
            match p.kind {
                crate::geometry::PaneKind::Sidebar => {
                    self.focus = Focus::Sidebar;
                    self.mode = Mode::Normal;
                    if let Some((_, item)) =
                        self.sidebar_rows.iter().find(|(ry, _)| *ry == y).cloned()
                    {
                        let items = self.sidebar_items();
                        if let Some(pos) = items.iter().position(|it| *it == item) {
                            self.sidebar.cursor = pos;
                            self.sidebar_activate();
                        }
                    }
                }
                crate::geometry::PaneKind::Members => {
                    self.focus = Focus::Members;
                    self.mode = Mode::Normal;
                    if let Some((idx, _)) = self
                        .member_rows
                        .iter()
                        .enumerate()
                        .find(|(_, (ry, _))| *ry == y)
                    {
                        self.members_cursor = idx;
                    }
                }
                crate::geometry::PaneKind::Deck(i) => {
                    self.focus = Focus::Deck(i);
                    self.mode = Mode::Normal;
                    if let Some(&(_, idx)) = self.deck_rows.iter().find(|(ry, _)| *ry == y) {
                        self.deck_cursor = idx;
                    }
                }
                crate::geometry::PaneKind::Chat => {
                    self.focus = Focus::Messages;
                    self.mode = Mode::Normal;
                }
            }
            break;
        }
        std::mem::take(&mut self.pending)
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
            self.autocomplete.clear();
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
        if !self.composer.is_empty() || !self.composer.attachments.is_empty() {
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
        self.apply_modal_outcome(m, outcome);
    }

    fn apply_modal_outcome(&mut self, m: Modal, outcome: ModalOutcome) {
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
        let Some(v) = self.store.voice.as_mut() else {
            return;
        };
        v.self_mute = !v.self_mute;
        let muted = v.self_mute;
        self.pending
            .push(Effect::Action(UiAction::SetSelfMute(muted)));
    }

    pub(crate) fn toggle_deaf(&mut self) {
        let Some(v) = self.store.voice.as_mut() else {
            return;
        };
        v.self_deaf = !v.self_deaf;
        let deaf = v.self_deaf;
        self.pending
            .push(Effect::Action(UiAction::SetSelfDeaf(deaf)));
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
                self.store.ring = CallRingState::Idle;
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
            CallRingState::OutgoingRinging { channel_id, .. } => {
                self.pending
                    .push(Effect::Action(UiAction::StopCallRing { channel_id }));
                self.pending.push(Effect::Action(UiAction::LeaveVoice));
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
        // Sidebar cursor on a voice channel (or nested voice occupant), else active call-capable channel.
        if self.focus == Focus::Sidebar {
            let voice_ch = match self.sidebar_items().get(self.sidebar.cursor).cloned() {
                Some(SideItem::Channel(id, _))
                    if self.store.channel(id).is_some_and(|c| c.kind.is_voice()) =>
                {
                    Some(id)
                }
                Some(SideItem::VoiceMember(ch_id, _, _)) => Some(ch_id),
                _ => None,
            };
            if let Some(id) = voice_ch {
                if self.store.voice.as_ref().is_some_and(|v| v.channel_id == id) {
                    self.pending.push(Effect::Action(UiAction::LeaveVoice));
                } else {
                    let g = self.store.channel(id).and_then(|c| c.guild_id);
                    self.pending.push(Effect::Action(UiAction::JoinVoice {
                        guild_id: g,
                        channel_id: id,
                    }));
                }
                return;
            }
        }
        if let Some(ch) = self.active_channel
            && let Some(c) = self.store.channel(ch)
            && c.kind.supports_voice_call()
        {
            if self.store.voice.as_ref().is_some_and(|v| v.channel_id == ch) {
                self.pending.push(Effect::Action(UiAction::LeaveVoice));
            } else {
                let g = c.guild_id;
                self.pending.push(Effect::Action(UiAction::JoinVoice {
                    guild_id: g,
                    channel_id: ch,
                }));
            }
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
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Stop {
            Pane(Focus),
            Composer,
        }
        let mut order: Vec<Stop> = Vec::new();
        if let Some(l) = &self.layout {
            for p in &l.panes {
                match p.kind {
                    crate::geometry::PaneKind::Sidebar => order.push(Stop::Pane(Focus::Sidebar)),
                    crate::geometry::PaneKind::Chat => {
                        order.push(Stop::Pane(Focus::Messages));
                        if !l.micro {
                            order.push(Stop::Composer);
                        }
                    }
                    crate::geometry::PaneKind::Members => order.push(Stop::Pane(Focus::Members)),
                    crate::geometry::PaneKind::Deck(i) => order.push(Stop::Pane(Focus::Deck(i))),
                }
            }
        }
        if order.is_empty() {
            order.push(Stop::Pane(Focus::Sidebar));
            order.push(Stop::Pane(Focus::Messages));
            order.push(Stop::Composer);
        }
        let cur = if self.mode == Mode::Insert {
            Stop::Composer
        } else {
            Stop::Pane(self.focus)
        };
        let i = order.iter().position(|s| *s == cur).unwrap_or(0);
        let n = order.len();
        let next = if dir >= 0 {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        match order[next] {
            Stop::Composer => {
                self.focus = Focus::Messages;
                self.mode = Mode::Insert;
            }
            Stop::Pane(f) => {
                self.autocomplete.clear();
                self.mode = Mode::Normal;
                self.focus = f;
            }
        }
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

    /// Flattened sidebar rows (DMs, folders, guilds, channels, and nested voice-channel occupants).
    #[must_use]
    pub fn sidebar_items(&self) -> Vec<SideItem> {
        let mut out = vec![SideItem::Home];
        if self.sidebar.expanded.contains(&None) {
            for c in self.store.dms_by_recency() {
                out.push(SideItem::Channel(c.id, 1));
                for p in self.store.effective_voice_occupants(c.id) {
                    out.push(SideItem::VoiceMember(c.id, p.user_id, 2));
                }
            }
        }
        let push_guild = |out: &mut Vec<SideItem>, gid: Snowflake, depth: u8| {
            out.push(SideItem::Guild(gid, depth));
            if self.sidebar.expanded.contains(&Some(gid)) {
                let mut hidden_parent: Option<Snowflake> = None;
                for c in self.store.sidebar_channels(gid) {
                    if c.kind == ChannelKind::GuildCategory {
                        hidden_parent = self.sidebar.collapsed.contains(&c.id).then_some(c.id);
                        out.push(SideItem::Channel(c.id, depth + 1));
                        continue;
                    }
                    if hidden_parent.is_some() && c.parent_id == hidden_parent {
                        continue;
                    }
                    let cdepth = depth
                        + if c.kind.is_thread() {
                            3
                        } else if c.parent_id.is_some() {
                            2
                        } else {
                            1
                        };
                    out.push(SideItem::Channel(c.id, cdepth));
                    if c.kind.is_voice() {
                        for p in self.store.effective_voice_occupants(c.id) {
                            out.push(SideItem::VoiceMember(c.id, p.user_id, cdepth + 1));
                        }
                    }
                }
            }
        };
        let mut placed: HashSet<Snowflake> = HashSet::new();
        for (idx, f) in self.store.folders.iter().enumerate() {
            let valid: Vec<Snowflake> = f
                .guild_ids
                .iter()
                .copied()
                .filter(|gid| self.store.guild(*gid).is_some() && !placed.contains(gid))
                .collect();
            if valid.is_empty() {
                continue;
            }
            let is_real_folder = f.id.is_some()
                || f.name.as_deref().is_some_and(|s| !s.is_empty())
                || valid.len() > 1;
            if is_real_folder {
                out.push(SideItem::Folder(idx));
                for gid in &valid {
                    placed.insert(*gid);
                }
                if !self.sidebar.collapsed_folders.contains(&idx) {
                    for gid in valid {
                        push_guild(&mut out, gid, 1);
                    }
                }
            } else {
                for gid in valid {
                    placed.insert(gid);
                    push_guild(&mut out, gid, 0);
                }
            }
        }
        for g in &self.store.guilds {
            if placed.insert(g.id) {
                push_guild(&mut out, g.id, 0);
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
            SideItem::Folder(idx) => {
                if !self.sidebar.collapsed_folders.remove(&idx) {
                    self.sidebar.collapsed_folders.insert(idx);
                }
            }
            SideItem::Guild(id, _) => {
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
                    self.open_channel(id);
                }
            }
            SideItem::VoiceMember(ch_id, _, _) => {
                self.select_channel(ch_id);
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
        let active_guild = store
            .channel(ch)
            .and_then(|c| c.guild_id)
            .or(self.active_guild);
        let res = crate::render::StoreResolver::new(store, active_guild);
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
        let active_guild = store
            .channel(ch)
            .and_then(|c| c.guild_id)
            .or(self.active_guild);
        let res = crate::render::StoreResolver::new(store, active_guild);
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
        let at_tail = s.state == ScrollState::FollowTail;
        self.scrolls.insert(ch, s);
        if at_tail {
            self.maybe_ack_channel(ch);
        }
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
            let author = self.store.author_display_name(&m);
            self.composer.target = ComposeTarget::Reply {
                message_id: m.id,
                author,
                snippet: crate::text::truncate(&snippet, 40, "…"),
                mention: true,
            };
            self.mode = Mode::Insert;
        }
    }

    fn edit_selected_own(&mut self) {
        let me = self.store.me.as_ref().map(|u| u.id);
        if let Some(m) = self.selected_message().cloned()
            && Some(m.author.id) == me
        {
            self.selected = Some(m.id);
            self.reveal_selected();
            self.composer.set_text(&m.content);
            self.composer.target = ComposeTarget::Edit { message_id: m.id };
            self.mode = Mode::Insert;
            return;
        }
        if self.selected.is_none() {
            self.edit_last_own();
        }
    }

    fn edit_last_own(&mut self) {
        let me = self.store.me.as_ref().map(|u| u.id);
        let msg = self
            .active_messages()
            .and_then(|ms| ms.iter().rev().find(|m| Some(m.author.id) == me))
            .cloned();
        if let Some(m) = msg {
            self.selected = Some(m.id);
            self.reveal_selected();
            self.composer.set_text(&m.content);
            self.composer.target = ComposeTarget::Edit { message_id: m.id };
            self.mode = Mode::Insert;
        }
    }

    fn delete_selected_own(&mut self) {
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
    }

    fn collect_message_urls(m: &Message) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut push_unique = |label: String, url: String| {
            if !url.is_empty() && !out.iter().any(|(_, u)| u == &url) {
                out.push((label, url));
            }
        };
        for a in &m.attachments {
            if !a.is_voice_message() {
                push_unique(a.filename.clone(), a.url.clone());
            }
        }
        for e in &m.embeds {
            if let Some(u) = &e.url {
                let lbl = e.title.clone().unwrap_or_else(|| u.clone());
                push_unique(lbl, u.clone());
            }
            if let Some(img) = &e.image {
                let lbl = e
                    .title
                    .clone()
                    .unwrap_or_else(|| "embed image".to_string());
                push_unique(lbl, img.url.clone());
            }
        }
        for token in m.content.split_whitespace() {
            let trimmed = token.trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | '[' | ']' | ',' | '.' | ';'));
            if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
                push_unique(trimmed.to_string(), trimmed.to_string());
            }
        }
        out
    }

    pub(crate) fn open_selected_media(&mut self) {
        let Some(m) = self.selected_message().cloned() else {
            self.play_selected_voice();
            return;
        };
        let urls = Self::collect_message_urls(&m);
        if urls.is_empty() {
            self.play_selected_voice();
        } else if urls.len() == 1 {
            if let Some((_, url)) = urls.into_iter().next() {
                self.pending_open_url = Some(url);
            }
        } else {
            self.open(Modal::attachments(urls));
        }
    }

    fn copy_selected_text(&mut self) {
        if let Some(m) = self.selected_message() {
            if !m.content.is_empty() {
                self.pending_copy_text = Some(m.content.clone());
            } else if let Some((_, url)) = Self::collect_message_urls(m).into_iter().next() {
                self.pending_copy_text = Some(url);
            }
        }
    }

    fn copy_selected_url(&mut self) {
        if let Some(m) = self.selected_message()
            && let Some((_, url)) = Self::collect_message_urls(m).into_iter().next()
        {
            self.pending_copy_text = Some(url);
        }
    }

    pub(crate) fn send(&mut self) {
        let Some(ch) = self.active_channel else {
            return;
        };
        if self.composer.is_empty() && self.composer.attachments.is_empty() {
            return;
        }
        // Built-in `/attach [path]` command for quick file/image attachment from the composer.
        let trimmed_cmd = self.composer.text().trim();
        if trimmed_cmd == "/attach" || trimmed_cmd.starts_with("/attach ") {
            let arg = trimmed_cmd.strip_prefix("/attach").unwrap_or("").trim().to_string();
            let _ = self.composer.take();
            self.autocomplete.clear();
            if arg.is_empty() {
                self.open_attach();
            } else {
                let base = self
                    .last_attach_dir
                    .clone()
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let resolved = crate::modals::expand_user_path(&arg, &base);
                if resolved.is_dir() {
                    self.last_attach_dir = Some(resolved.clone());
                    self.open(Modal::attach(Some(resolved)));
                } else if resolved.is_file() {
                    if let Some(parent) = resolved.parent() {
                        self.last_attach_dir = Some(parent.to_path_buf());
                    }
                    let _ = self.stage_attachment(resolved.to_string_lossy().into_owned());
                } else {
                    self.store.banners.push(crate::state::Banner {
                        scope: "attach".to_string(),
                        message: format!(
                            "File not found: {}",
                            crate::sanitize::sanitize_single_line(&resolved.to_string_lossy())
                        ),
                        recoverable: true,
                    });
                }
            }
            return;
        }
        if self.composer.char_len() > self.cfg.max_message_len() {
            self.open(Modal::too_long(self.cfg.max_message_len()));
            return;
        }
        self.autocomplete.clear();
        let target = self.composer.target.clone();
        let attachments = std::mem::take(&mut self.composer.attachments);
        let raw = self.composer.take();
        let content = crate::composer::expand_outgoing_mentions(&raw, &self.store, self.active_guild);
        self.composer.target = ComposeTarget::New;
        match target {
            ComposeTarget::Edit { message_id } => {
                self.pending.push(Effect::Action(UiAction::EditMessage {
                    channel_id: ch,
                    message_id,
                    content,
                }));
            }
            ComposeTarget::Reply {
                message_id,
                mention,
                ..
            } => {
                self.pending.push(Effect::Action(UiAction::SendMessage {
                    channel_id: ch,
                    content,
                    reply_to: Some(message_id),
                    mention_replied_user: mention,
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
        self.autocomplete.clear();
        let limit = self.cfg.max_message_len();
        let raw = self.composer.take();
        let text = crate::composer::expand_outgoing_mentions(&raw, &self.store, self.active_guild);
        let (reply, mention) = match &self.composer.target {
            ComposeTarget::Reply {
                message_id,
                mention,
                ..
            } => (Some(*message_id), *mention),
            _ => (None, true),
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
                    mention_replied_user: mention,
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
                            mention_replied_user: mention,
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
                mention_replied_user: mention,
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
            if ctrl(&k, 'a') {
                self.open_attach();
                return;
            }
        }
        // Autocomplete popup intercepts navigation and acceptance before composer keys.
        if self.autocomplete.is_open() {
            match k.code {
                KeyCode::Esc => {
                    self.autocomplete.clear();
                    return;
                }
                KeyCode::Up => {
                    self.autocomplete.move_up();
                    return;
                }
                KeyCode::Down => {
                    self.autocomplete.move_down();
                    return;
                }
                KeyCode::Tab => {
                    self.autocomplete.accept(&mut self.composer);
                    return;
                }
                KeyCode::Enter if !alt && !k.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.autocomplete.accept(&mut self.composer);
                    return;
                }
                _ => {}
            }
        }
        let mut edited = false;
        let mut typed_char = false;
        match k.code {
            KeyCode::Esc => {
                if self.composer.target != ComposeTarget::New {
                    self.composer.target = ComposeTarget::New;
                } else {
                    self.mode = Mode::Normal;
                }
                return;
            }
            KeyCode::Enter if alt || k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.composer.insert_char('\n');
                edited = true;
            }
            KeyCode::Enter => {
                self.send();
                return;
            }
            KeyCode::Char('j') if ctl => {
                self.composer.insert_char('\n');
                edited = true;
            }
            KeyCode::Char('m') if alt => {
                if let ComposeTarget::Reply { mention, .. } = &mut self.composer.target {
                    *mention = !*mention;
                }
                return;
            }
            KeyCode::Tab => {
                self.refresh_autocomplete();
                if self.autocomplete.is_open() {
                    self.autocomplete.accept(&mut self.composer);
                } else if let ComposeTarget::Reply { mention, .. } = &mut self.composer.target {
                    *mention = !*mention;
                } else if self.composer.is_empty() {
                    self.cycle_focus(1);
                }
                return;
            }
            KeyCode::BackTab => {
                self.cycle_focus(-1);
                return;
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                edited = true;
            }
            KeyCode::Char('h') if ctl => {
                self.composer.backspace();
                edited = true;
            }
            KeyCode::Delete => {
                self.composer.delete();
                edited = true;
            }
            KeyCode::Char('d') if ctl => {
                self.composer.delete();
                edited = true;
            }
            KeyCode::Left => {
                self.composer.left();
                edited = true;
            }
            KeyCode::Char('b') if ctl => {
                self.composer.left();
                edited = true;
            }
            KeyCode::Right => {
                self.composer.right();
                edited = true;
            }
            KeyCode::Char('f') if ctl => {
                self.composer.right();
                edited = true;
            }
            KeyCode::Home => {
                self.composer.home();
                edited = true;
            }
            KeyCode::Char('a') if ctl => {
                self.composer.home();
                edited = true;
            }
            KeyCode::End => {
                self.composer.end();
                edited = true;
            }
            KeyCode::Char('e') if ctl => {
                self.composer.end();
                edited = true;
            }
            KeyCode::Char('u') if ctl => {
                self.composer.kill_to_bol();
                edited = true;
            }
            KeyCode::Char('w') if ctl => {
                self.composer.kill_word_back();
                edited = true;
            }
            KeyCode::Char('y') if ctl => {
                self.composer.yank();
                edited = true;
            }
            KeyCode::Char('t') if ctl => {
                self.composer.transpose();
                edited = true;
            }
            KeyCode::Char('x') if ctl => self.ctrl_x = true,
            KeyCode::Char('k') if ctl => match self.cfg.ctrl_k {
                CtrlKMode::Kill => {
                    self.composer.kill_to_eol();
                    edited = true;
                }
                CtrlKMode::Switcher => self.open(Modal::switcher(self)),
                CtrlKMode::Dwim => {
                    if self.composer.is_empty() || self.composer.at_end() {
                        self.open(Modal::switcher(self));
                    } else {
                        self.composer.kill_to_eol();
                        edited = true;
                    }
                }
            },
            KeyCode::Char('g') if ctl => self.open_gif(),
            KeyCode::Char('v') if ctl => self.pending.push(Effect::PasteClipboard),
            KeyCode::Char('a') if alt => self.open_attach(),
            KeyCode::Char('x') if alt => {
                let _ = self.pop_staged_attachment();
            }
            KeyCode::Char('o') if ctl => self.toggle_mute(),
            KeyCode::Char('n') if ctl => self.jump_unread(1),
            KeyCode::Char('p') if ctl => self.jump_unread(-1),
            KeyCode::Char('r') if ctl => self.reply_to_selected(),
            KeyCode::Char('b') if alt => {
                self.composer.word_left();
                edited = true;
            }
            KeyCode::Char('f') if alt => {
                self.composer.word_right();
                edited = true;
            }
            KeyCode::Char('d') if alt => {
                self.composer.kill_word_fwd();
                edited = true;
            }
            KeyCode::Up if self.composer.is_empty() => self.edit_last_own(),
            KeyCode::PageUp => self.scroll_page(-1),
            KeyCode::PageDown => self.scroll_page(1),
            KeyCode::Char(c) if !ctl && !alt => {
                self.composer.insert_char(c);
                edited = true;
                typed_char = true;
            }
            _ => {}
        }
        if edited {
            self.refresh_autocomplete();
        }
        if typed_char {
            self.maybe_send_typing();
        }
    }

    fn refresh_autocomplete(&mut self) {
        self.autocomplete.refresh(
            &self.composer,
            &self.store,
            self.active_channel,
            self.active_guild,
        );
    }

    fn maybe_send_typing(&mut self) {
        let Some(ch) = self.active_channel else {
            return;
        };
        if self.now_ms == 0 {
            return;
        }
        if self.last_typing_sent_ms != 0
            && self.now_ms.saturating_sub(self.last_typing_sent_ms) < 8_000
        {
            return;
        }
        self.last_typing_sent_ms = self.now_ms;
        self.pending
            .push(Effect::Action(UiAction::SendTyping { channel_id: ch }));
    }

    fn scroll_page(&mut self, dir: isize) {
        let h = self
            .layout
            .as_ref()
            .map_or(10, |l| l.messages.height.max(2));
        self.scroll_lines(dir * isize::try_from(h.saturating_sub(1).max(1)).unwrap_or(1));
    }

    fn jump_top(&mut self) {
        match self.focus {
            Focus::Sidebar => self.sidebar.cursor = 0,
            Focus::Members => self.members_cursor = 0,
            Focus::Deck(_) => self.deck_cursor = 0,
            Focus::Messages => {
                if let Some(ch) = self.active_channel {
                    if let Some((params, pkey, height)) = crate::render::messages_view_params(self)
                    {
                        let store = &self.store;
                        if let Some(msgs) = store.messages.get(&ch)
                            && !msgs.messages.is_empty()
                        {
                            let active_guild = store
                                .channel(ch)
                                .and_then(|c| c.guild_id)
                                .or(self.active_guild);
                            let res = crate::render::StoreResolver::new(store, active_guild);
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
            }
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        if ctrl(&k, 'k') {
            self.open(Modal::switcher(self));
            return;
        }
        if ctrl(&k, 'f') {
            self.open(Modal::search());
            return;
        }
        if ctrl(&k, 'd') {
            self.open(Modal::Dave);
            return;
        }
        if ctrl(&k, 't') {
            let m = Modal::theme(self);
            self.open(m);
            return;
        }
        if ctrl(&k, 'g') {
            self.open_gif();
            return;
        }
        if ctrl(&k, 'v') {
            self.pending.push(Effect::PasteClipboard);
            return;
        }
        if k.modifiers.contains(KeyModifiers::ALT) && k.code == KeyCode::Char('a') {
            self.open_attach();
            return;
        }
        if k.modifiers.contains(KeyModifiers::ALT) && k.code == KeyCode::Char('x') {
            let _ = self.pop_staged_attachment();
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
        if k.modifiers.contains(KeyModifiers::ALT) && k.code == KeyCode::Char('r') {
            self.gdm_key('R');
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
                self.jump_top();
                return;
            }
            KeyCode::End => {
                if let Some(ch) = self.active_channel {
                    self.scrolls.insert(ch, Scroll::default());
                    self.maybe_ack_channel(ch);
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
                self.delete_selected_own();
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
            'j' => self.nav(1),
            'k' => self.nav(-1),
            'g' => self.jump_top(),
            '/' => self.open(Modal::switcher(self)),
            'f' => self.open(Modal::search()),
            'i' => self.mode = Mode::Insert,
            'r' => self.reply_to_selected(),
            'e' => self.edit_selected_own(),
            'E' => self.edit_last_own(),
            'd' => self.delete_selected_own(),
            'D' => self.toggle_deaf(),
            'R' => {
                if self.focus == Focus::Sidebar && self.gdm_target_channel().is_some() {
                    self.gdm_key('R');
                } else {
                    let target = self
                        .selected
                        .or_else(|| self.active_messages().and_then(|m| m.back()).map(|m| m.id));
                    if let Some(t) = target {
                        self.open(Modal::react(t));
                    }
                }
            }
            't' => self.open(Modal::threads(self)),
            'p' => self.open(Modal::pins(self)),
            'T' => {
                let m = Modal::theme(self);
                self.open(m);
            }
            'G' => self.open_gif(),
            'A' => self.open_attach(),
            ']' => self.jump_unread(1),
            '[' => self.jump_unread(-1),
            'm' => self.toggle_mute(),
            'v' if in_voice_ctx => self.open(Modal::Dave),
            'v' => self.join_voice(),
            'a' => self.call(),
            'h' if self.focus == Focus::Sidebar
                && self.store.voice.is_none()
                && matches!(self.store.ring, CallRingState::Idle) =>
            {
                self.sidebar_collapse();
            }
            'h' => self.decline(),
            'l' if self.focus == Focus::Sidebar => self.sidebar_activate(),
            's' => self.screenshare(),
            'w' => self.toggle_watch(),
            'W' => self.toggle_video_sink(),
            'M' => self.toggle_members(),
            'V' => self.open(Modal::Voice),
            'c' => self.focus = Focus::Sidebar,
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
            'n' | '+' | '-' | 'L' => self.gdm_key(c),
            'o' => self.open_selected_media(),
            'y' => self.copy_selected_text(),
            'u' => self.copy_selected_url(),
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

    fn gdm_target_channel(&self) -> Option<Snowflake> {
        let candidate = if self.focus == Focus::Sidebar {
            match self.sidebar_items().get(self.sidebar.cursor) {
                Some(SideItem::Channel(id, _))
                    if self
                        .store
                        .channel(*id)
                        .is_some_and(|c| c.kind.is_group_dm()) =>
                {
                    Some(*id)
                }
                _ => self.active_channel,
            }
        } else {
            self.active_channel
        };
        candidate.filter(|id| {
            self.store
                .channel(*id)
                .is_some_and(|c| c.kind.is_group_dm())
        })
    }

    fn gdm_key(&mut self, c: char) {
        let ch = if self.focus == Focus::Sidebar {
            match self.sidebar_items().get(self.sidebar.cursor) {
                Some(SideItem::Channel(id, _))
                    if self
                        .store
                        .channel(*id)
                        .is_some_and(|ch| ch.kind.is_group_dm()) =>
                {
                    Some(*id)
                }
                Some(SideItem::Home) => self.gdm_target_channel(),
                Some(SideItem::Channel(id, _)) => Some(*id),
                _ => self.active_channel,
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
            Some(SideItem::Folder(idx)) => {
                self.sidebar.collapsed_folders.insert(*idx);
            }
            Some(SideItem::Guild(id, _)) => {
                self.sidebar.expanded.remove(&Some(*id));
            }
            Some(SideItem::Channel(id, _)) => {
                // Jump to the owning guild row (or collapse category if on a category).
                let is_cat = self
                    .store
                    .channel(*id)
                    .is_some_and(|c| c.kind == ChannelKind::GuildCategory);
                if is_cat && !self.sidebar.collapsed.contains(id) {
                    self.sidebar.collapsed.insert(*id);
                    return;
                }
                let gid = self.store.channel(*id).and_then(|c| c.guild_id);
                if let Some(pos) = items.iter().position(|i| match (i, gid) {
                    (SideItem::Guild(g, _), Some(x)) => *g == x,
                    (SideItem::Home, None) => true,
                    _ => false,
                }) {
                    self.sidebar.cursor = pos;
                }
            }
            Some(SideItem::VoiceMember(ch_id, _, _)) => {
                if let Some(pos) = items
                    .iter()
                    .position(|i| matches!(i, SideItem::Channel(c, _) if c == ch_id))
                {
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
