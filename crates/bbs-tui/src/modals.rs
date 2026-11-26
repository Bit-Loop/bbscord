//! Modals and overlays (§2.5): quick switcher, theme picker (live preview), GIF picker,
//! reactions, threads/pins/search, Group DM management, DAVE verification, help, confirms.

use std::collections::HashSet;

use bbs_core::{ChannelKind, DaveSessionState, GifSearchQuery, Snowflake, UiAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use crate::app::{App, Effect};
use crate::draw::{self, Ctx};
use crate::geometry::modal_rect;
use crate::sanitize::sanitize_single_line;
use crate::text::{self, str_width};
use crate::theme::{self, Role};

/// What a picker row does when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickAction {
    /// Open a channel.
    Channel(Snowflake),
    /// Open a guild (`None` = DMs).
    Guild(Option<Snowflake>),
    /// Theme index in `builtin_specs()` / catalog.
    Theme(usize),
    /// React to a message.
    React(Snowflake, String),
    /// Jump to a message in the active channel.
    Jump(Snowflake),
    /// Add a user to a Group DM.
    GdmAdd(Snowflake, Snowflake),
    /// Remove a user from a Group DM.
    GdmRemove(Snowflake, Snowflake),
    /// Toggle a user for a new Group DM (multi-select).
    GdmNewUser(Snowflake),
    /// Run a main-menu command.
    Menu(MenuCmd),
    /// Open an attachment or URL via the host runner.
    OpenUrl(String),
}

/// Main menu commands (reachable from any input device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCmd {
    /// Quick switcher.
    Switcher,
    /// Theme picker.
    Theme,
    /// GIF picker.
    Gif,
    /// Voice deck.
    Voice,
    /// Help.
    Help,
    /// Quit.
    Quit,
}

/// A picker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickItem {
    /// Label (sanitized, normalized at draw time).
    pub label: String,
    /// Right-aligned detail.
    pub detail: String,
    /// Action.
    pub action: PickAction,
}

/// Generic fuzzy list picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// Title.
    pub title: String,
    /// Query prompt (e.g. `Jump>`).
    pub prompt: String,
    /// Query text.
    pub query: String,
    /// Items.
    pub items: Vec<PickItem>,
    /// Indices into `items` after filtering.
    pub filtered: Vec<usize>,
    /// Cursor within `filtered`.
    pub cursor: usize,
    /// Multi-selection (Group DM creation).
    pub multi: Option<HashSet<usize>>,
    /// Hint line.
    pub hint: String,
}

impl Picker {
    fn new(title: &str, prompt: &str, items: Vec<PickItem>, hint: &str) -> Self {
        let mut p = Self {
            title: title.to_string(),
            prompt: prompt.to_string(),
            query: String::new(),
            filtered: Vec::new(),
            items,
            cursor: 0,
            multi: None,
            hint: hint.to_string(),
        };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            let m = SkimMatcherV2::default();
            let mut scored: Vec<(i64, usize)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| m.fuzzy_match(&it.label, &self.query).map(|s| (s, i)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        }
        self.cursor = self.cursor.min(self.filtered.len().saturating_sub(1));
    }

    /// Currently highlighted item.
    #[must_use]
    pub fn current(&self) -> Option<&PickItem> {
        self.filtered
            .get(self.cursor)
            .and_then(|i| self.items.get(*i))
    }
}

/// GIF picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GifState {
    /// Correlation id of the latest dispatched request.
    pub request_id: u64,
    /// Query text.
    pub query: String,
    /// Selected tile/row cursor.
    pub cursor: usize,
    /// Monotonic tick deadline (ms) for debounced live search (`None` = idle).
    pub pending_search_at_ms: Option<u64>,
    /// Columns in the rendered GIF grid (`1` in text-list mode).
    pub grid_cols: usize,
    /// Whether an in-flight search request is still waiting for a reply.
    pub loading: bool,
}

/// Confirm dialog kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// Quit bbscord.
    Quit,
    /// Delete own message.
    Delete(Snowflake),
    /// Leave a Group DM.
    Leave(Snowflake),
    /// Oversized message (limit).
    TooLong(usize),
}

/// All modals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// Fuzzy list picker.
    Picker(Picker),
    /// Theme picker (live preview).
    Theme(Picker),
    /// GIF picker.
    Gif(GifState),
    /// Help.
    Help(u16),
    /// Confirmation (`selected_yes` defaults to `false`, i.e., "No" focused for safety).
    Confirm {
        /// Confirmation variant.
        kind: ConfirmKind,
        /// Whether the `[ Yes ]` button is currently focused (`false` = `[ No ]`).
        selected_yes: bool,
    },
    /// Rename Group DM text input.
    Rename(Snowflake, String),
    /// Message search input.
    Search(String),
    /// Voice / Stage overlay (non-Ultra).
    Voice,
    /// Members overlay (Standard).
    Members,
    /// DAVE verification.
    Dave,
}

/// Result of a modal key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalOutcome {
    /// Stay open.
    Keep,
    /// Close (theme preview reverts).
    Close,
    /// Close and keep changes (theme commit).
    CloseCommit,
    /// Replace with another modal.
    Replace(Box<Modal>),
}

fn chan_sigil(app: &App, kind: ChannelKind) -> &'static str {
    if app.glyphs().is_ascii() || app.cfg.charset == bbs_core::ResolvedCharset::VtSafe {
        kind.ascii_sigil()
    } else {
        kind.utf8_sigil()
    }
}

impl Modal {
    /// Quit confirmation ("No" focused by default).
    #[must_use]
    pub fn confirm_quit() -> Self {
        Self::Confirm {
            kind: ConfirmKind::Quit,
            selected_yes: false,
        }
    }

    /// Delete confirmation ("No" focused by default).
    #[must_use]
    pub fn confirm_delete(id: Snowflake) -> Self {
        Self::Confirm {
            kind: ConfirmKind::Delete(id),
            selected_yes: false,
        }
    }

    /// Leave-GDM confirmation ("No" focused by default).
    #[must_use]
    pub fn confirm_leave(id: Snowflake) -> Self {
        Self::Confirm {
            kind: ConfirmKind::Leave(id),
            selected_yes: false,
        }
    }

    /// Oversized message prompt.
    #[must_use]
    pub fn too_long(limit: usize) -> Self {
        Self::Confirm {
            kind: ConfirmKind::TooLong(limit),
            selected_yes: false,
        }
    }

    /// Help.
    #[must_use]
    pub fn help() -> Self {
        Self::Help(0)
    }

    /// Search.
    #[must_use]
    pub fn search() -> Self {
        Self::Search(String::new())
    }

    /// GIF picker for a request.
    #[must_use]
    pub fn gif(request_id: u64) -> Self {
        Self::Gif(GifState {
            request_id,
            query: String::new(),
            cursor: 0,
            pending_search_at_ms: None,
            grid_cols: 1,
            loading: true,
        })
    }

    /// Attachment / URL picker for messages with multiple links or files.
    #[must_use]
    pub fn attachments(entries: Vec<(String, String)>) -> Self {
        let items = entries
            .into_iter()
            .map(|(label, url)| PickItem {
                label: sanitize_single_line(&label).into_owned(),
                detail: sanitize_single_line(&url).into_owned(),
                action: PickAction::OpenUrl(url),
            })
            .collect();
        Self::Picker(Picker::new("Open", "Open>", items, "Enter open  Esc close"))
    }

    /// Rename input.
    #[must_use]
    pub fn gdm_rename(id: Snowflake) -> Self {
        Self::Rename(id, String::new())
    }

    /// Quick switcher over DMs, guilds and channels.
    #[must_use]
    pub fn switcher(app: &App) -> Self {
        let mut items = Vec::new();
        for c in app.store.dms_by_recency() {
            let label = format!(
                "{}{}",
                chan_sigil(app, c.kind),
                sanitize_single_line(&c.display_label())
            );
            let detail = if c.mention_count > 0 {
                format!("@{}", c.mention_count)
            } else if c.unread_count > 0 {
                format!("{}", c.unread_count)
            } else {
                String::new()
            };
            items.push(PickItem {
                label,
                detail,
                action: PickAction::Channel(c.id),
            });
        }
        for g in app.store.ordered_guilds() {
            let gname = sanitize_single_line(&g.name).into_owned();
            items.push(PickItem {
                label: format!("*{gname}"),
                detail: String::new(),
                action: PickAction::Guild(Some(g.id)),
            });
            for c in app.store.sidebar_channels(g.id) {
                if c.kind == ChannelKind::GuildCategory {
                    continue;
                }
                let label = format!(
                    "{}{} ({gname})",
                    chan_sigil(app, c.kind),
                    sanitize_single_line(&c.name)
                );
                let detail = if c.mention_count > 0 {
                    format!("@{}", c.mention_count)
                } else if c.unread_count > 0 {
                    format!("{}", c.unread_count)
                } else {
                    String::new()
                };
                items.push(PickItem {
                    label,
                    detail,
                    action: PickAction::Channel(c.id),
                });
            }
        }
        Self::Picker(Picker::new("Jump", "Jump>", items, "Enter open  Esc close"))
    }

    /// Theme picker over the freshly rescanned catalog (built-ins, user, `btop/…`).
    pub fn theme(app: &mut App) -> Self {
        app.rescan_themes();
        let current = app.theme().name().to_string();
        let items: Vec<PickItem> = app
            .theme_catalog()
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| PickItem {
                label: e.label.clone(),
                detail: match e.origin {
                    theme::ThemeOrigin::BuiltIn => String::new(),
                    theme::ThemeOrigin::User => "user".to_string(),
                    theme::ThemeOrigin::Btop => "btop".to_string(),
                },
                action: PickAction::Theme(i),
            })
            .collect();
        let mut p = Picker::new("Theme", "Theme>", items, "Enter keep  Esc revert");
        p.cursor = app.theme_catalog().position(&current).unwrap_or(0);
        Self::Theme(p)
    }

    /// Reaction picker (searchable Unicode emoji catalog).
    #[must_use]
    pub fn react(msg: Snowflake) -> Self {
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for &(name, emoji) in crate::composer::COMMON_EMOJIS {
            if seen.insert(emoji) {
                items.push(PickItem {
                    label: format!("{emoji} :{name}:"),
                    detail: String::new(),
                    action: PickAction::React(msg, emoji.to_string()),
                });
            }
        }
        Self::Picker(Picker::new(
            "React",
            "React>",
            items,
            "Enter react  Esc close",
        ))
    }

    /// Threads of the active channel.
    #[must_use]
    pub fn threads(app: &App) -> Self {
        let mut items = Vec::new();
        if let Some(ch) = app.active_channel {
            for chans in app.store.channels.values() {
                for t in chans
                    .iter()
                    .filter(|t| t.kind.is_thread() && t.parent_id == Some(ch))
                {
                    items.push(PickItem {
                        label: format!(
                            "{}{}",
                            chan_sigil(app, t.kind),
                            sanitize_single_line(&t.name)
                        ),
                        detail: String::new(),
                        action: PickAction::Channel(t.id),
                    });
                }
            }
        }
        Self::Picker(Picker::new(
            "Threads",
            "Thread>",
            items,
            "Enter open  Esc close",
        ))
    }

    /// Pinned messages of the active channel.
    #[must_use]
    pub fn pins(app: &App) -> Self {
        let items = app
            .active_messages()
            .map(|ms| {
                ms.iter()
                    .filter(|m| m.pinned)
                    .map(|m| PickItem {
                        label: format!(
                            "{}: {}",
                            sanitize_single_line(m.author.display_name()),
                            sanitize_single_line(&m.content)
                        ),
                        detail: crate::messages::fmt_time(m.id, app.utc_offset_min),
                        action: PickAction::Jump(m.id),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self::Picker(Picker::new("Pins", "Pins>", items, "Enter jump  Esc close"))
    }

    fn known_users(app: &App, exclude: &[Snowflake]) -> Vec<(Snowflake, String)> {
        let me = app.store.me.as_ref().map(|u| u.id);
        let mut v: Vec<(Snowflake, String)> = app
            .store
            .users
            .values()
            .filter(|u| Some(u.id) != me && !u.bot && !exclude.contains(&u.id))
            .map(|u| (u.id, sanitize_single_line(u.display_name()).into_owned()))
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1));
        v
    }

    /// New Group DM: multi-select up to 9 others.
    #[must_use]
    pub fn gdm_new(app: &App) -> Self {
        let items = Self::known_users(app, &[])
            .into_iter()
            .map(|(id, n)| PickItem {
                label: n,
                detail: String::new(),
                action: PickAction::GdmNewUser(id),
            })
            .collect();
        let mut p = Picker::new(
            "New Group DM",
            "Add>",
            items,
            "Space pick (max 9)  Enter create",
        );
        p.multi = Some(HashSet::new());
        Self::Picker(p)
    }

    /// Add a recipient.
    #[must_use]
    pub fn gdm_add(app: &App, ch: Snowflake) -> Self {
        let existing: Vec<Snowflake> = app
            .store
            .channel(ch)
            .map(|c| c.recipients.iter().map(|u| u.id).collect())
            .unwrap_or_default();
        let items = Self::known_users(app, &existing)
            .into_iter()
            .map(|(id, n)| PickItem {
                label: n,
                detail: String::new(),
                action: PickAction::GdmAdd(ch, id),
            })
            .collect();
        Self::Picker(Picker::new(
            "Add to Group DM",
            "Add>",
            items,
            "Enter add  Esc close",
        ))
    }

    /// Remove a recipient (owner only; shown greyed otherwise).
    #[must_use]
    pub fn gdm_remove(app: &App, ch: Snowflake) -> Self {
        let owner = app.store.channel(ch).and_then(|c| c.owner_id);
        let me = app.store.me.as_ref().map(|u| u.id);
        let is_owner = owner.is_some() && owner == me;
        let items = app
            .store
            .channel(ch)
            .map(|c| {
                c.recipients
                    .iter()
                    .map(|u| PickItem {
                        label: sanitize_single_line(u.display_name()).into_owned(),
                        detail: if is_owner {
                            String::new()
                        } else {
                            "owner only".to_string()
                        },
                        action: PickAction::GdmRemove(ch, u.id),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self::Picker(Picker::new(
            "Remove from Group DM",
            "Remove>",
            items,
            if is_owner {
                "Enter remove  Esc close"
            } else {
                "Only the owner can remove"
            },
        ))
    }

    /// Feed pasted text into a text input.
    pub fn paste(&mut self, s: &str) {
        let one = s.replace('\n', " ");
        match self {
            Self::Picker(p) | Self::Theme(p) => {
                p.query.push_str(&one);
                p.refilter();
            }
            Self::Gif(g) => {
                g.query.push_str(&one);
                g.cursor = 0;
                g.pending_search_at_ms = Some(0);
            }
            Self::Rename(_, t) | Self::Search(t) => t.push_str(&one),
            _ => {}
        }
    }

    /// Text-input cursor offset (cells into the query) for modals with an input.
    #[must_use]
    pub fn has_input(&self) -> bool {
        matches!(
            self,
            Self::Picker(_) | Self::Theme(_) | Self::Gif(_) | Self::Rename(..) | Self::Search(_)
        )
    }
}

fn edit_line(s: &mut String, k: &KeyEvent) -> bool {
    match k.code {
        KeyCode::Backspace => {
            s.pop();
            true
        }
        KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            s.pop();
            true
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            s.clear();
            true
        }
        KeyCode::Char(c)
            if !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            s.push(c);
            true
        }
        _ => false,
    }
}

fn list_nav(cursor: &mut usize, len: usize, k: &KeyEvent) -> bool {
    match k.code {
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            if len > 0 {
                *cursor = (*cursor + 1).min(len - 1);
            }
            true
        }
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            *cursor = cursor.saturating_sub(1);
            true
        }
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if len > 0 {
                *cursor = (*cursor + 1).min(len - 1);
            }
            true
        }
        KeyCode::PageUp => {
            *cursor = cursor.saturating_sub(10);
            true
        }
        KeyCode::PageDown => {
            if len > 0 {
                *cursor = (*cursor + 10).min(len - 1);
            }
            true
        }
        _ => false,
    }
}

fn run_pick(app: &mut App, a: PickAction) -> ModalOutcome {
    match a {
        PickAction::Channel(id) => {
            app.open_channel(id);
        }
        PickAction::Guild(g) => {
            app.select_guild(g);
            app.focus = crate::app::Focus::Sidebar;
            let items = app.sidebar_items();
            if let Some(pos) = items.iter().position(|i| match (i, g) {
                (crate::app::SideItem::Guild(x, _), Some(y)) => *x == y,
                (crate::app::SideItem::Home, None) => true,
                _ => false,
            }) {
                app.sidebar.cursor = pos;
            }
        }
        PickAction::Theme(_) => return ModalOutcome::CloseCommit,
        PickAction::React(msg, emoji) => {
            if let Some(ch) = app.active_channel {
                let mine = app
                    .active_messages()
                    .and_then(|ms| ms.iter().find(|m| m.id == msg))
                    .and_then(|m| m.reactions.iter().find(|r| r.emoji_name == emoji))
                    .is_some_and(|r| r.me);
                app.pending.push(Effect::Action(UiAction::ToggleReaction {
                    channel_id: ch,
                    message_id: msg,
                    emoji,
                    add: !mine,
                }));
            }
        }
        PickAction::Jump(id) => {
            app.selected = Some(id);
            app.focus = crate::app::Focus::Messages;
            app.reveal_selected();
        }
        PickAction::GdmAdd(ch, user) => {
            app.pending
                .push(Effect::Action(UiAction::AddGroupDmRecipient {
                    channel_id: ch,
                    user_id: user,
                }));
        }
        PickAction::GdmRemove(ch, user) => {
            let owner = app.store.channel(ch).and_then(|c| c.owner_id);
            if owner.is_some() && owner == app.store.me.as_ref().map(|u| u.id) {
                app.pending
                    .push(Effect::Action(UiAction::RemoveGroupDmRecipient {
                        channel_id: ch,
                        user_id: user,
                    }));
            } else {
                app.pending.push(Effect::Bell);
                return ModalOutcome::Keep;
            }
        }
        PickAction::GdmNewUser(_) => return ModalOutcome::Keep,
        PickAction::OpenUrl(url) => {
            app.pending_open_url = Some(url);
        }
        PickAction::Menu(cmd) => {
            let next = match cmd {
                MenuCmd::Switcher => Modal::switcher(app),
                MenuCmd::Theme => {
                    app.theme_before_preview = Some(app.theme.clone());
                    Modal::theme(app)
                }
                MenuCmd::Gif => {
                    app.gif_req = app.gif_req.wrapping_add(1);
                    app.pending.push(Effect::Action(UiAction::SearchGifs {
                        request_id: app.gif_req,
                        query: GifSearchQuery::default(),
                    }));
                    Modal::gif(app.gif_req)
                }
                MenuCmd::Voice => Modal::Voice,
                MenuCmd::Help => Modal::help(),
                MenuCmd::Quit => Modal::confirm_quit(),
            };
            return ModalOutcome::Replace(Box::new(next));
        }
    }
    ModalOutcome::Close
}

/// Main menu (F10 / Micro MAIN MENU equivalents).
#[must_use]
pub fn main_menu() -> Modal {
    let items = [
        ("Jump to channel", MenuCmd::Switcher),
        ("Voice", MenuCmd::Voice),
        ("GIF search", MenuCmd::Gif),
        ("Theme", MenuCmd::Theme),
        ("Help", MenuCmd::Help),
        ("Quit", MenuCmd::Quit),
    ]
    .iter()
    .map(|(l, c)| PickItem {
        label: (*l).to_string(),
        detail: String::new(),
        action: PickAction::Menu(*c),
    })
    .collect();
    Modal::Picker(Picker::new(
        "Menu",
        "Menu>",
        items,
        "Enter select  Esc close",
    ))
}

fn preview_theme(app: &mut App, p: &Picker) {
    if let Some(PickItem {
        action: PickAction::Theme(i),
        ..
    }) = p.current()
    {
        app.apply_catalog(*i);
    }
}

/// Execute a confirmation modal choice (`yes = true` confirms, `false` cancels).
pub fn confirm_choice(app: &mut App, kind: ConfirmKind, yes: bool) -> ModalOutcome {
    if !yes {
        return ModalOutcome::Close;
    }
    match kind {
        ConfirmKind::Quit => {
            app.pending.push(Effect::Action(UiAction::Quit));
            app.pending.push(Effect::Quit);
        }
        ConfirmKind::Delete(id) => {
            if let Some(ch) = app.active_channel {
                app.pending.push(Effect::Action(UiAction::DeleteMessage {
                    channel_id: ch,
                    message_id: id,
                }));
            }
        }
        ConfirmKind::Leave(id) => {
            app.pending
                .push(Effect::Action(UiAction::LeaveGroupDm { channel_id: id }));
        }
        ConfirmKind::TooLong(_) => {}
    }
    ModalOutcome::Close
}

/// Dispatch an immediate GIF search for `g`, clearing any pending debounce timer.
pub fn dispatch_gif_search(app: &mut App, g: &mut GifState) {
    g.pending_search_at_ms = None;
    g.loading = true;
    app.gif_req = app.gif_req.wrapping_add(1);
    g.request_id = app.gif_req;
    g.cursor = 0;
    let q = GifSearchQuery {
        query: g.query.clone(),
        trending: g.query.is_empty(),
        ..GifSearchQuery::default()
    };
    app.pending.push(Effect::Action(UiAction::SearchGifs {
        request_id: g.request_id,
        query: q,
    }));
}

fn gif_items_for_modal(app: &App) -> Vec<bbs_core::GifItem> {
    app.store
        .gif_results
        .as_ref()
        .map(|(_, v, _)| v.clone())
        .unwrap_or_default()
}

/// Activate a clicked row/tile index inside the currently open modal.
pub fn activate_row(app: &mut App, m: &mut Modal, index: usize) -> ModalOutcome {
    match m {
        Modal::Picker(p) => {
            if index >= p.filtered.len() {
                return ModalOutcome::Keep;
            }
            p.cursor = index;
            if let Some(sel) = p.multi.as_mut() {
                if let Some(i) = p.filtered.get(p.cursor).copied()
                    && !sel.remove(&i)
                    && sel.len() < 9
                {
                    sel.insert(i);
                }
                return ModalOutcome::Keep;
            }
            match p.current().map(|c| c.action.clone()) {
                Some(a) => run_pick(app, a),
                None => ModalOutcome::Keep,
            }
        }
        Modal::Theme(p) => {
            if index >= p.filtered.len() {
                return ModalOutcome::Keep;
            }
            p.cursor = index;
            preview_theme(app, p);
            ModalOutcome::CloseCommit
        }
        Modal::Gif(g) => {
            let items = gif_items_for_modal(app);
            if let (Some(item), Some(ch)) = (items.get(index), app.active_channel) {
                g.cursor = index;
                let reply = match &app.composer.target {
                    crate::composer::ComposeTarget::Reply { message_id, .. } => Some(*message_id),
                    _ => None,
                };
                app.pending.push(Effect::Action(UiAction::SendGif {
                    channel_id: ch,
                    gif: item.clone(),
                    reply_to: reply,
                }));
                return ModalOutcome::Close;
            }
            ModalOutcome::Keep
        }
        Modal::Voice | Modal::Dave => {
            app.deck_cursor = index;
            ModalOutcome::Keep
        }
        Modal::Members => {
            app.members_cursor = index;
            ModalOutcome::Keep
        }
        _ => ModalOutcome::Keep,
    }
}

/// Scroll the currently open modal by `delta` lines (negative = up, positive = down).
pub fn scroll_modal(app: &mut App, m: &mut Modal, delta: i16) {
    let steps = usize::from(delta.unsigned_abs());
    match m {
        Modal::Picker(p) => {
            if delta < 0 {
                p.cursor = p.cursor.saturating_sub(steps);
            } else if !p.filtered.is_empty() {
                p.cursor = (p.cursor + steps).min(p.filtered.len() - 1);
            }
        }
        Modal::Theme(p) => {
            if delta < 0 {
                p.cursor = p.cursor.saturating_sub(steps);
            } else if !p.filtered.is_empty() {
                p.cursor = (p.cursor + steps).min(p.filtered.len() - 1);
            }
            preview_theme(app, p);
        }
        Modal::Gif(g) => {
            let items = gif_items_for_modal(app);
            let stride = g.grid_cols.max(app.gif_grid_cols).max(1);
            if delta < 0 {
                g.cursor = g.cursor.saturating_sub(stride);
            } else if !items.is_empty() {
                g.cursor = (g.cursor + stride).min(items.len() - 1);
            }
        }
        Modal::Help(scroll) => {
            if delta < 0 {
                *scroll = scroll.saturating_sub(delta.unsigned_abs());
            } else {
                *scroll = scroll.saturating_add(delta.unsigned_abs());
            }
        }
        Modal::Voice | Modal::Dave => {
            if delta < 0 {
                app.deck_cursor = app.deck_cursor.saturating_sub(steps);
            } else {
                app.deck_cursor = app.deck_cursor.saturating_add(steps);
            }
        }
        Modal::Members => {
            if delta < 0 {
                app.members_cursor = app.members_cursor.saturating_sub(steps);
            } else {
                app.members_cursor = app.members_cursor.saturating_add(steps);
            }
        }
        _ => {}
    }
}

/// Route a key to a modal.
pub fn handle_key(app: &mut App, m: &mut Modal, k: KeyEvent) -> ModalOutcome {
    if k.code == KeyCode::Esc {
        return ModalOutcome::Close;
    }
    match m {
        Modal::Picker(p) => {
            if list_nav(&mut p.cursor, p.filtered.len(), &k) {
                return ModalOutcome::Keep;
            }
            if k.code == KeyCode::Char(' ')
                && let Some(sel) = p.multi.as_mut()
            {
                if let Some(i) = p.filtered.get(p.cursor).copied()
                    && !sel.remove(&i)
                    && sel.len() < 9
                {
                    sel.insert(i);
                }
                return ModalOutcome::Keep;
            }
            if k.code == KeyCode::Enter {
                if let Some(sel) = &p.multi {
                    let ids: Vec<Snowflake> = sel
                        .iter()
                        .filter_map(|i| match p.items.get(*i).map(|it| &it.action) {
                            Some(PickAction::GdmNewUser(u)) => Some(*u),
                            _ => None,
                        })
                        .collect();
                    if !ids.is_empty() {
                        app.pending.push(Effect::Action(UiAction::CreateGroupDm {
                            recipient_ids: ids,
                            name: None,
                        }));
                        return ModalOutcome::Close;
                    }
                    return ModalOutcome::Keep;
                }
                return match p.current().map(|c| c.action.clone()) {
                    Some(a) => run_pick(app, a),
                    None => ModalOutcome::Keep,
                };
            }
            if edit_line(&mut p.query, &k) {
                p.refilter();
            }
            ModalOutcome::Keep
        }
        Modal::Theme(p) => {
            if list_nav(&mut p.cursor, p.filtered.len(), &k) {
                preview_theme(app, p);
                return ModalOutcome::Keep;
            }
            if k.code == KeyCode::Enter {
                preview_theme(app, p);
                return ModalOutcome::CloseCommit;
            }
            if edit_line(&mut p.query, &k) {
                p.refilter();
                preview_theme(app, p);
            }
            ModalOutcome::Keep
        }
        Modal::Gif(g) => {
            let items = gif_items_for_modal(app);
            let cols = g.grid_cols.max(app.gif_grid_cols).max(1);
            match k.code {
                KeyCode::Left => g.cursor = g.cursor.saturating_sub(1),
                KeyCode::Right => {
                    if !items.is_empty() {
                        g.cursor = (g.cursor + 1).min(items.len() - 1);
                    }
                }
                KeyCode::Up => g.cursor = g.cursor.saturating_sub(cols),
                KeyCode::Down => {
                    if !items.is_empty() {
                        g.cursor = (g.cursor + cols).min(items.len() - 1);
                    }
                }
                KeyCode::Enter => {
                    if g.pending_search_at_ms.is_some()
                        || (!g.query.is_empty()
                            && (items.is_empty() || k.modifiers.contains(KeyModifiers::ALT)))
                    {
                        dispatch_gif_search(app, g);
                        return ModalOutcome::Keep;
                    }
                    if let (Some(item), Some(ch)) = (items.get(g.cursor), app.active_channel) {
                        let reply = match &app.composer.target {
                            crate::composer::ComposeTarget::Reply { message_id, .. } => {
                                Some(*message_id)
                            }
                            _ => None,
                        };
                        app.pending.push(Effect::Action(UiAction::SendGif {
                            channel_id: ch,
                            gif: item.clone(),
                            reply_to: reply,
                        }));
                        return ModalOutcome::Close;
                    }
                }
                KeyCode::Tab => {
                    dispatch_gif_search(app, g);
                }
                _ => {
                    if edit_line(&mut g.query, &k) {
                        // Debounce live search by 300ms; Enter fires immediately.
                        g.cursor = 0;
                        g.pending_search_at_ms = Some(app.now_ms.saturating_add(300));
                    }
                }
            }
            ModalOutcome::Keep
        }
        Modal::Help(scroll) => {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::Enter | KeyCode::Char('q' | '?') => return ModalOutcome::Close,
                _ => {}
            }
            ModalOutcome::Keep
        }
        Modal::Confirm { kind, selected_yes } => match *kind {
            ConfirmKind::TooLong(_) => match k.code {
                KeyCode::Char('f') => {
                    app.send_as_file();
                    ModalOutcome::Close
                }
                KeyCode::Char('s') => {
                    app.send_split();
                    ModalOutcome::Close
                }
                KeyCode::Char('n') | KeyCode::Enter => ModalOutcome::Close,
                _ => ModalOutcome::Keep,
            },
            other => match k.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Char('h' | 'l') => {
                    *selected_yes = !*selected_yes;
                    ModalOutcome::Keep
                }
                KeyCode::Char('y' | 'Y') => confirm_choice(app, other, true),
                KeyCode::Enter => confirm_choice(app, other, *selected_yes),
                KeyCode::Char('n' | 'N' | 'q') => ModalOutcome::Close,
                _ => ModalOutcome::Keep,
            },
        },
        Modal::Rename(id, t) => {
            if k.code == KeyCode::Enter {
                let name = t.trim().to_string();
                if !name.is_empty() {
                    app.pending.push(Effect::Action(UiAction::RenameGroupDm {
                        channel_id: *id,
                        name,
                    }));
                }
                return ModalOutcome::Close;
            }
            edit_line(t, &k);
            ModalOutcome::Keep
        }
        Modal::Search(q) => {
            if k.code == KeyCode::Enter {
                let needle = q.to_lowercase();
                let items: Vec<PickItem> = app
                    .active_messages()
                    .map(|ms| {
                        ms.iter()
                            .rev()
                            .filter(|m| m.content.to_lowercase().contains(&needle))
                            .map(|m| PickItem {
                                label: format!(
                                    "{}: {}",
                                    sanitize_single_line(&app.store.author_display_name(m)),
                                    sanitize_single_line(&m.content)
                                ),
                                detail: crate::messages::fmt_time(m.id, app.utc_offset_min),
                                action: PickAction::Jump(m.id),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return ModalOutcome::Replace(Box::new(Modal::Picker(Picker::new(
                    "Results",
                    "Filter>",
                    items,
                    "Enter jump  Esc close",
                ))));
            }
            edit_line(q, &k);
            ModalOutcome::Keep
        }
        Modal::Voice | Modal::Dave => {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.deck_cursor = app.deck_cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => app.deck_cursor += 1,
                KeyCode::Char('+' | '=') => app.volume_step(10, false),
                KeyCode::Char('-') => app.volume_step(-10, false),
                KeyCode::Char('0') => app.volume_reset(),
                KeyCode::Char('x') => app.volume_step(0, true),
                KeyCode::Char('m') => app.toggle_mute(),
                KeyCode::Char('d') => app.toggle_deaf(),
                KeyCode::Char('w') => app.toggle_watch(),
                KeyCode::Char('W') => app.toggle_video_sink(),
                KeyCode::Char('s') => app.screenshare(),
                KeyCode::Char('h') => {
                    app.decline();
                    return ModalOutcome::Close;
                }
                KeyCode::Char('v') if matches!(m, Modal::Voice) => {
                    return ModalOutcome::Replace(Box::new(Modal::Dave));
                }
                KeyCode::Char('v') => {
                    // Mark the selected participant verified locally.
                    if let Some(v) = &app.store.voice
                        && let Some(p) = v
                            .participants
                            .get(app.deck_cursor.min(v.participants.len().saturating_sub(1)))
                    {
                        let id = p.user_id;
                        if !app.dave_verified.remove(&id) {
                            app.dave_verified.insert(id);
                        }
                    }
                }
                KeyCode::Enter | KeyCode::Char('q') => return ModalOutcome::Close,
                _ => {}
            }
            ModalOutcome::Keep
        }
        Modal::Members => {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.members_cursor = app.members_cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => app.members_cursor += 1,
                KeyCode::Enter | KeyCode::Char('q') => return ModalOutcome::Close,
                _ => {}
            }
            ModalOutcome::Keep
        }
    }
}

/// Keymap rows for help: (action, NORMAL, F-key, Ctrl).
pub const KEYMAP: &[(&str, &str, &str, &str)] = &[
    ("Quick switcher", "/", "", "Ctrl+K"),
    ("Navigate / scroll", "j/k  ↑/↓  g/End", "", "PgUp / PgDn"),
    ("Compose / send / newline", "i / Enter", "", "Ctrl+J / Alt+Enter"),
    ("Autocomplete (@user :emoji: #chan)", "Tab / Enter (Insert)", "", ""),
    ("Reply (Tab/Alt+M toggle ping)", "r", "", "Ctrl+R"),
    ("Edit selected / last own message", "e / E", "", ""),
    ("Delete message (safe confirm)", "d / Del", "", ""),
    ("React / threads / pins", "R / t / p", "", ""),
    ("Open attachment/URL / play voice", "o", "", ""),
    ("Copy message / URL", "y / u", "", ""),
    ("Search messages", "f", "", "Ctrl+F"),
    ("Theme switcher", "T", "F2", "Ctrl+T"),
    ("GIF picker", "G", "F5", "Ctrl+G"),
    ("Next / prev unread", "] / [", "", "Ctrl+N / Ctrl+P"),
    ("Mute / deafen (in voice)", "m / D", "F7 / F8", "Ctrl+O"),
    ("Join/leave voice / call / hang up", "v / a / h", "F9", ""),
    ("Screenshare / watch / sink", "s / w / W", "", ""),
    ("Members / voice deck / DAVE", "M / V / Ctrl+D", "F3 / F4", "Ctrl+D"),
    ("Volume -/+ / reset / local mute", "- + 0 x (Stage)", "", ""),
    ("Group DM new/add/remove/rename/leave", "n + - R(side) L", "", ""),
    ("Focus pane / Mouse click & wheel", "1-6 / Tab / Click", "F6", ""),
    ("Help / Repaint / Suspend", "?", "F1", "Ctrl+L / Ctrl+Z"),
    ("Quit", "q", "F10", "Ctrl+Q"),
    ("Edit in $EDITOR", "", "", "Ctrl+X Ctrl+E"),
];

/// Draw the top modal. Returns the text-cursor position for input modals.
pub fn render(
    app: &mut App,
    m: &Modal,
    area: Rect,
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
) -> Option<(u16, u16)> {
    let (want_w, want_h, title) = match m {
        Modal::Picker(p) | Modal::Theme(p) => (60, 20, p.title.clone()),
        Modal::Gif(_) => (86, 24, "GIF".to_string()),
        Modal::Help(_) => (78, 30, "Help".to_string()),
        Modal::Confirm { .. } => (48, 8, "Confirm".to_string()),
        Modal::Rename(..) => (48, 5, "Rename Group DM".to_string()),
        Modal::Search(_) => (56, 5, "Search".to_string()),
        Modal::Voice => (72, 22, "Voice".to_string()),
        Modal::Members => (36, 24, "Members".to_string()),
        Modal::Dave => (64, 18, "DAVE verification".to_string()),
    };
    let mr = modal_rect(area, want_w, want_h);
    app.modal_rect = Some(mr.rect);
    // Prevent underlying chat/deck images from painting over the modal.
    app.image_slots
        .retain(|s| s.visible.intersection(mr.rect).is_empty());
    let inner = if mr.sheet {
        draw::fill(buf, mr.rect, ctx.base());
        mr.rect
    } else {
        draw::boxed(buf, ctx, mr.rect, &title, Role::BoxMembers)
    };
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    let hint_row = mr.hint_row && inner.height >= 3;
    match m {
        Modal::Picker(p) | Modal::Theme(p) => {
            render_picker(app, p, inner, mr.sheet, hint_row, buf, ctx)
        }
        Modal::Gif(g) => render_gif(app, g, inner, mr.sheet, hint_row, buf, ctx),
        Modal::Help(scroll) => {
            render_help(*scroll, inner, buf, ctx);
            None
        }
        Modal::Confirm { kind, selected_yes } => {
            render_confirm(app, *kind, *selected_yes, inner, buf, ctx);
            None
        }
        Modal::Rename(_, t) | Modal::Search(t) => {
            let prompt = if matches!(m, Modal::Rename(..)) {
                "Name> "
            } else {
                "Find> "
            };
            let shown = format!("{prompt}{}", ctx.n(t));
            let w = draw::put(buf, inner.x, inner.y, &shown, inner.width, ctx.s(Role::Fg));
            Some((inner.x + w.min(inner.width.saturating_sub(1)), inner.y))
        }
        Modal::Voice => {
            crate::panes::render_stage(app, inner, buf, ctx, true);
            None
        }
        Modal::Members => {
            crate::panes::render_members(app, inner, buf, ctx, true);
            None
        }
        Modal::Dave => {
            render_dave(app, inner, buf, ctx);
            None
        }
    }
}

fn render_confirm(
    app: &mut App,
    kind: ConfirmKind,
    selected_yes: bool,
    inner: Rect,
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
) {
    if let ConfirmKind::TooLong(n) = kind {
        let msg = format!("Over {n} chars: [s]plit [f]ile message.txt [n] edit");
        let y = inner.y + inner.height.saturating_sub(1) / 2;
        for (i, l) in text::wrap(&ctx.n(&msg), usize::from(inner.width))
            .iter()
            .enumerate()
        {
            let ry = y + u16::try_from(i).unwrap_or(0);
            if ry >= inner.bottom() {
                break;
            }
            draw::put(buf, inner.x, ry, &l.text, inner.width, ctx.s(Role::Fg));
        }
        return;
    }

    let prompt = match kind {
        ConfirmKind::Quit => "Quit bbscord?",
        ConfirmKind::Delete(_) => "Delete this message?",
        ConfirmKind::Leave(_) => "Leave this Group DM?",
        ConfirmKind::TooLong(_) => "",
    };
    let wrapped = text::wrap(&ctx.n(prompt), usize::from(inner.width));
    let mut y = inner.y;
    for l in &wrapped {
        if y >= inner.bottom() {
            break;
        }
        draw::put(
            buf,
            inner.x,
            y,
            &l.text,
            inner.width,
            ctx.s(Role::Fg).add_modifier(Modifier::BOLD),
        );
        y += 1;
    }

    // Draw clickable [ No ] / [ Yes ] buttons.
    let btn_y = (y + 1).min(inner.bottom().saturating_sub(1));
    if btn_y >= inner.y && btn_y < inner.bottom() {
        let no_lbl = "[ No ]";
        let yes_lbl = "[ Yes ]";
        let no_w = u16::try_from(no_lbl.len()).unwrap_or(6);
        let yes_w = u16::try_from(yes_lbl.len()).unwrap_or(7);
        let no_st = if !selected_yes {
            ctx.s(Role::Selected).add_modifier(Modifier::BOLD)
        } else {
            ctx.s(Role::Inactive)
        };
        let yes_st = if selected_yes {
            ctx.s(Role::Mention)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            ctx.s(Role::Inactive)
        };
        let no_x = inner.x;
        let drawn_no = draw::put(buf, no_x, btn_y, no_lbl, inner.width, no_st);
        let gap = 2u16;
        let yes_x = no_x.saturating_add(drawn_no).saturating_add(gap);
        let yes_avail = inner.right().saturating_sub(yes_x);
        let drawn_yes = if yes_avail > 0 {
            draw::put(buf, yes_x, btn_y, yes_lbl, yes_avail, yes_st)
        } else {
            0
        };
        app.confirm_buttons = Some((
            Rect::new(no_x, btn_y, drawn_no.min(no_w), 1),
            Rect::new(yes_x, btn_y, drawn_yes.min(yes_w), 1),
        ));
    }

    let hint_y = inner.bottom().saturating_sub(1);
    if hint_y > btn_y {
        draw::put(
            buf,
            inner.x,
            hint_y,
            &ctx.n("←/→/Tab select  Enter confirm  y/n  Esc cancel"),
            inner.width,
            ctx.s(Role::Inactive),
        );
    }
}

fn render_picker(
    app: &mut App,
    p: &Picker,
    inner: Rect,
    sheet: bool,
    hint_row: bool,
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
) -> Option<(u16, u16)> {
    // Row 0: title+query merged in sheet mode, query only when boxed.
    let _ = sheet;
    let prompt = format!("{} ", p.prompt);
    let q = ctx.n(&p.query);
    let line = format!("{prompt}{q}");
    let w = draw::put(
        buf,
        inner.x,
        inner.y,
        &line,
        inner.width,
        ctx.s(Role::Hotkey),
    );
    let count = format!("{}/{}", p.filtered.len(), p.items.len());
    let cw = u16::try_from(str_width(&count)).unwrap_or(0);
    if inner.width > w + cw + 1 {
        draw::put_right(
            buf,
            inner.x,
            inner.y,
            &count,
            inner.width,
            ctx.s(Role::Inactive),
        );
    }
    let cursor = Some((inner.x + w.min(inner.width.saturating_sub(1)), inner.y));
    let list_top = inner.y + 1;
    let list_h = inner.height.saturating_sub(1 + u16::from(hint_row));
    if list_h == 0 {
        return cursor;
    }
    let lh = usize::from(list_h);
    let start = p.cursor.saturating_sub(lh.saturating_sub(1));
    for (row, fi) in p.filtered.iter().enumerate().skip(start).take(lh) {
        let Some(it) = p.items.get(*fi) else { continue };
        let y = list_top + u16::try_from(row - start).unwrap_or(0);
        app.modal_rows
            .push((Rect::new(inner.x, y, inner.width, 1), row));
        let picked = p.multi.as_ref().is_some_and(|s| s.contains(fi));
        let mark = if p.multi.is_some() {
            if picked { "[x] " } else { "[ ] " }
        } else {
            ""
        };
        let label = format!("{mark}{}", ctx.n(&it.label));
        let selected = row == p.cursor;
        let dw = u16::try_from(str_width(&it.detail)).unwrap_or(0);
        let lw = inner.width.saturating_sub(if dw > 0 { dw + 1 } else { 0 });
        draw::list_row(buf, ctx, inner.x, y, lw, &label, selected, ctx.s(Role::Fg));
        if dw > 0 && inner.width > dw {
            let st = if selected {
                ctx.s(Role::Selected)
            } else {
                ctx.s(Role::Hotkey)
            };
            draw::put_right(
                buf,
                inner.x + lw,
                y,
                &ctx.n(&it.detail),
                inner.width - lw,
                st,
            );
        }
    }
    if hint_row {
        draw::put(
            buf,
            inner.x,
            inner.bottom() - 1,
            &ctx.n(&p.hint),
            inner.width,
            ctx.s(Role::Inactive),
        );
    }
    cursor
}

fn render_gif(
    app: &mut App,
    g: &GifState,
    inner: Rect,
    sheet: bool,
    hint_row: bool,
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
) -> Option<(u16, u16)> {
    let _ = sheet;
    let line = format!("GIF> {}", ctx.n(&g.query));
    let w = draw::put(
        buf,
        inner.x,
        inner.y,
        &line,
        inner.width,
        ctx.s(Role::Hotkey),
    );
    let results_match = app
        .store
        .gif_results
        .as_ref()
        .is_some_and(|(id, _, _)| *id == g.request_id);
    let err_msg = app
        .store
        .gif_error
        .as_ref()
        .filter(|(id, _)| *id == g.request_id)
        .map(|(_, e)| e.clone());
    let is_loading = (g.loading && !results_match && err_msg.is_none())
        || g.pending_search_at_ms.is_some();
    if is_loading {
        let ind = ctx.n("loading…");
        let iw = u16::try_from(str_width(&ind)).unwrap_or(0);
        if inner.width > w + iw + 1 {
            draw::put_right(
                buf,
                inner.x,
                inner.y,
                &ind,
                inner.width,
                ctx.s(Role::Inactive),
            );
        }
    }
    let cursor = Some((inner.x + w.min(inner.width.saturating_sub(1)), inner.y));
    let items = gif_items_for_modal(app);
    let body = Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1 + u16::from(hint_row)),
    );
    if let Some(err) = err_msg
        && (items.is_empty() || !results_match)
    {
        app.gif_grid_cols = 1;
        if let Some(Modal::Gif(top)) = app.modals.last_mut() {
            top.grid_cols = 1;
        }
        let msg = format!("GIF error: {err} (Tab to retry)");
        for (i, l) in text::wrap(&ctx.n(&msg), usize::from(body.width))
            .iter()
            .enumerate()
        {
            let ry = body.y + u16::try_from(i).unwrap_or(0);
            if ry >= body.bottom() {
                break;
            }
            draw::put(
                buf,
                body.x,
                ry,
                &l.text,
                body.width,
                ctx.s(Role::Mention).add_modifier(Modifier::BOLD),
            );
        }
    } else if items.is_empty() {
        app.gif_grid_cols = 1;
        if let Some(Modal::Gif(top)) = app.modals.last_mut() {
            top.grid_cols = 1;
        }
        let status = if is_loading {
            "searching…"
        } else {
            "No GIFs found. Try another query or press Tab."
        };
        draw::put(
            buf,
            body.x,
            body.y,
            &ctx.n(status),
            body.width,
            ctx.s(Role::Inactive),
        );
    } else {
        let tw: u16 = 20;
        let text_list = body.height < 8
            || body.width < tw
            || ctx.cfg.graphics == bbs_core::GraphicsTier::TextTagOnly;
        if text_list {
            app.gif_grid_cols = 1;
            if let Some(Modal::Gif(top)) = app.modals.last_mut() {
                top.grid_cols = 1;
            }
            let lh = usize::from(body.height);
            let start = g.cursor.saturating_sub(lh.saturating_sub(1));
            for (i, it) in items.iter().enumerate().skip(start).take(lh) {
                let y = body.y + u16::try_from(i - start).unwrap_or(0);
                app.modal_rows
                    .push((Rect::new(body.x, y, body.width, 1), i));
                let label = format!(
                    "[{}] {}x{}",
                    ctx.n(&sanitize_single_line(&it.title)),
                    it.width,
                    it.height
                );
                draw::list_row(
                    buf,
                    ctx,
                    body.x,
                    y,
                    body.width,
                    &label,
                    i == g.cursor,
                    ctx.s(Role::Fg),
                );
            }
        } else {
            let th = 8u16.min(body.height.saturating_sub(2)).max(1);
            let cols = usize::from(((body.width + 1) / (tw + 1)).max(1));
            app.gif_grid_cols = cols;
            if let Some(Modal::Gif(top)) = app.modals.last_mut() {
                top.grid_cols = cols;
            }
            let rows_vis = usize::from((body.height / (th + 1)).max(1));
            let cur_row = g.cursor / cols;
            let first_row = cur_row.saturating_sub(rows_vis - 1);
            for (i, it) in items.iter().enumerate() {
                let r = i / cols;
                if r < first_row || r >= first_row + rows_vis {
                    continue;
                }
                let c = i % cols;
                let x = body.x + u16::try_from(c).unwrap_or(0) * (tw + 1);
                let y = body.y + u16::try_from(r - first_row).unwrap_or(0) * (th + 1);
                let tile = Rect::new(x, y, tw, th);
                app.modal_rows
                    .push((Rect::new(x, y, tw, th.saturating_add(1)), i));
                let selected = i == g.cursor;
                crate::panes::render_image_box(
                    app,
                    buf,
                    ctx,
                    tile,
                    &it.preview_url,
                    &format!("[{}]", sanitize_single_line(&it.title)),
                    it.preview_rgba_frames.as_ref().and_then(|f| f.first()),
                );
                let visible = tile.intersection(body).intersection(buf.area);
                if visible.width > 0 && visible.height > 0 {
                    app.image_slots.push(crate::app::ImageSlot {
                        url: it.preview_url.clone(),
                        rect: tile,
                        visible,
                        clip_top: visible.y.saturating_sub(tile.y),
                        animated: true,
                        focused: selected,
                    });
                }
                let cap = text::truncate(
                    &ctx.n(&sanitize_single_line(&it.title)),
                    usize::from(tw),
                    ctx.g.ellipsis,
                );
                let st = if selected {
                    ctx.s(Role::Selected)
                } else {
                    ctx.s(Role::Inactive)
                };
                draw::put_fit(buf, x, y + th, &cap, tw, st);
            }
        }
    }
    if hint_row {
        draw::put(
            buf,
            inner.x,
            inner.bottom() - 1,
            &ctx.n("type to search (300ms)  Arrows move  Enter search/send  Tab retry  Esc close"),
            inner.width,
            ctx.s(Role::Inactive),
        );
    }
    cursor
}

fn render_help(scroll: u16, inner: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    let w = inner.width;
    let c1 = (w / 2).max(1);
    let c2 = (w.saturating_sub(c1)) / 2;
    let c3 = w.saturating_sub(c1 + c2);
    let header = ctx.s(Role::Title).add_modifier(Modifier::BOLD);
    draw::put_fit(buf, inner.x, inner.y, &ctx.n("Action"), c1, header);
    draw::put_fit(buf, inner.x + c1, inner.y, &ctx.n("Normal"), c2, header);
    draw::put_fit(
        buf,
        inner.x + c1 + c2,
        inner.y,
        &ctx.n("F / Ctrl"),
        c3,
        header,
    );
    let rows = usize::from(inner.height.saturating_sub(1));
    let start = usize::from(scroll).min(KEYMAP.len().saturating_sub(1));
    for (i, (a, n, f, c)) in KEYMAP.iter().enumerate().skip(start).take(rows) {
        let y = inner.y + 1 + u16::try_from(i - start).unwrap_or(0);
        draw::put_fit(buf, inner.x, y, &ctx.n(a), c1, ctx.s(Role::Fg));
        draw::put_fit(buf, inner.x + c1, y, &ctx.n(n), c2, ctx.s(Role::Hotkey));
        let fc = match (f.is_empty(), c.is_empty()) {
            (false, false) => format!("{f} {c}"),
            (false, true) => (*f).to_string(),
            _ => (*c).to_string(),
        };
        draw::put_fit(
            buf,
            inner.x + c1 + c2,
            y,
            &ctx.n(&fc),
            c3,
            ctx.s(Role::Inactive),
        );
    }
}

fn render_dave(app: &mut App, inner: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    let Some(v) = app.store.voice.clone() else {
        draw::put(
            buf,
            inner.x,
            inner.y,
            &ctx.n("Not in a voice session."),
            inner.width,
            ctx.s(Role::Inactive),
        );
        return;
    };
    let mut y = inner.y;
    let (state, code) = match &v.dave {
        DaveSessionState::Active {
            epoch,
            protocol_version,
            verification_code,
        } => (
            format!("E2EE active · epoch {epoch} · v{protocol_version}"),
            verification_code.clone(),
        ),
        DaveSessionState::Handshaking => ("Handshaking…".to_string(), String::new()),
        DaveSessionState::Disabled => (
            "E2EE not negotiated (transport encryption only)".to_string(),
            String::new(),
        ),
        DaveSessionState::Degraded { reason } => (
            format!("DOWNGRADED: {}", sanitize_single_line(reason)),
            String::new(),
        ),
    };
    draw::put(
        buf,
        inner.x,
        y,
        &ctx.n(&state),
        inner.width,
        ctx.s(Role::Title),
    );
    y += 2;
    if !code.is_empty() {
        let digits: String = code.chars().filter(char::is_ascii_alphanumeric).collect();
        let groups: Vec<String> = digits
            .as_bytes()
            .chunks(5)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        draw::put(
            buf,
            inner.x,
            y,
            &ctx.n("Voice privacy code:"),
            inner.width,
            ctx.s(Role::Inactive),
        );
        y += 1;
        let per_line = usize::from((inner.width / 6).max(1));
        for chunk in groups.chunks(per_line) {
            draw::put(
                buf,
                inner.x,
                y,
                &chunk.join(" "),
                inner.width,
                ctx.s(Role::Hotkey).add_modifier(Modifier::BOLD),
            );
            y += 1;
        }
        y += 1;
    }
    for (i, p) in v.participants.iter().enumerate() {
        if y >= inner.bottom().saturating_sub(1) {
            break;
        }
        app.modal_rows
            .push((Rect::new(inner.x, y, inner.width, 1), i));
        let verified = p.dave_verified || app.dave_verified.contains(&p.user_id);
        let mark = if verified {
            ctx.g.check
        } else {
            ctx.g.unverified
        };
        let label = format!("{mark} {}", sanitize_single_line(&p.display_name));
        draw::list_row(
            buf,
            ctx,
            inner.x,
            y,
            inner.width,
            &ctx.n(&label),
            i == app.deck_cursor,
            ctx.s(Role::Fg),
        );
        y += 1;
    }
    draw::put(
        buf,
        inner.x,
        inner.bottom() - 1,
        &ctx.n("v mark verified  Esc close"),
        inner.width,
        ctx.s(Role::Inactive),
    );
}
