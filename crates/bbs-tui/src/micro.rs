//! Micro16x2 pager (§3): a 6-key (`↑ ↓ ← → Enter Esc`) state machine that uses every cell of a
//! `16×2` HD44780 LCD and scales to `20×4` and narrow-tall panes.

use bbs_core::{CallRingState, ChannelKind, Message, Snowflake, UiAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use crate::app::{App, Effect};
use crate::composer::ComposeTarget;
use crate::draw::{self, Ctx};
use crate::markdown;
use crate::sanitize::{sanitize_remote, sanitize_single_line};
use crate::text::{self, str_width};
use crate::theme::Role;

/// Micro screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MicroMode {
    /// Paged reading.
    #[default]
    Read,
    /// Guild/channel navigation.
    Nav,
    /// Main menu.
    Main,
    /// Per-message menu.
    Msg,
    /// Composer.
    Compose,
    /// Voice status.
    Voice,
    /// Voice actions.
    VoiceMenu,
    /// GIF results.
    Gif,
    /// GIF query editing.
    GifQuery,
    /// Theme cycling.
    Theme,
    /// Message search query.
    Search,
}

/// Navigation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavLevel {
    /// DMs + guilds.
    #[default]
    Root,
    /// Inside DMs (`None`) or a guild.
    In(Option<Snowflake>),
}

/// Pager state.
#[derive(Debug, Clone, Default)]
pub struct MicroState {
    /// Screen.
    pub mode: MicroMode,
    /// Message index (None = newest).
    pub msg: Option<usize>,
    /// Chunk within the message.
    pub chunk: usize,
    /// Nav level.
    pub level: NavLevel,
    /// Nav cursor.
    pub cursor: usize,
    /// Fuzzy filter / text query.
    pub filter: String,
    /// Menu cursor.
    pub menu: usize,
    /// Auto-read.
    pub auto_read: bool,
    /// Last auto-advance.
    pub last_advance_ms: u64,
    /// Esc pressed once in compose.
    pub esc_armed: bool,
    /// GIF cursor.
    pub gif: usize,
    /// GIF request id.
    pub gif_req: u64,
    /// Voice participant cursor.
    pub voice: usize,
    /// Theme cursor.
    pub theme: usize,
    /// Theme before preview (name).
    pub theme_prev: Option<String>,
    /// Theme before preview (spec, restored on Esc).
    pub theme_spec_prev: Option<bbs_core::ThemeSpec>,
}

impl MicroState {
    /// Enter compose mode.
    pub fn enter_compose(&mut self) {
        self.mode = MicroMode::Compose;
        self.esc_armed = false;
    }
}

const MAIN_ITEMS: [&str; 8] = [
    "Channels",
    "Compose",
    "Voice",
    "GIF search",
    "Search",
    "Theme",
    "Auto-read",
    "Quit",
];
const MSG_ITEMS: [&str; 7] = [
    "New msg",
    "Reply",
    "React +1",
    "Play voice",
    "Thread",
    "GIF",
    "Auto-read",
];
const VOICE_ITEMS: [&str; 6] = ["Mute", "Deafen", "Vol +10", "Vol -10", "Verify", "Leave"];

/// Rows: `(status, content_rows, hint)`.
fn rows_budget(rows: u16) -> (bool, u16, bool) {
    match rows {
        0 => (false, 0, false),
        1 => (false, 1, false),
        2 => (true, 1, false),
        3 => (true, 1, true),
        4 => (true, 2, true),
        r => (true, r - 2, true),
    }
}

/// Plain text of a message for paging, with synthesized tails.
fn message_text(app: &App, m: &Message) -> String {
    let ctx = app.cfg.text_ctx();
    let clean = sanitize_remote(&m.content);
    let store = &app.store;
    let md = markdown::render_markdown(&clean, &ctx, &|k, id| {
        crate::render::resolve_mention(store, k, id)
    });
    let mut s: String = md
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|sp| sp.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let imgs = m.attachments.iter().filter(|a| a.is_image()).count();
    if imgs > 0 {
        s.push_str(&format!(" [+{imgs} img]"));
    }
    for a in m.attachments.iter().filter(|a| !a.is_image()) {
        if let Some(v) = &a.voice_message {
            s.push_str(&format!(
                " [voice {}]",
                crate::messages::fmt_dur(v.duration_secs)
            ));
        } else {
            s.push_str(&format!(" [file {}]", sanitize_single_line(&a.filename)));
        }
    }
    for e in &m.embeds {
        if let Some(t) = &e.title {
            s.push_str(&format!(" [emb:{}]", sanitize_single_line(t)));
        } else if e.image.is_some() || e.video.is_some() {
            s.push_str(" [emb:media]");
        }
    }
    if !m.reactions.is_empty() {
        let chips: Vec<String> = m
            .reactions
            .iter()
            .map(|r| {
                let e = if r.emoji_id.is_some() {
                    format!(":{}:", r.emoji_name)
                } else {
                    r.emoji_name.clone()
                };
                format!("{}x{}", sanitize_single_line(&e), r.count)
            })
            .collect();
        s.push_str(&format!(" {}", chips.join(" ")));
    }
    if s.trim().is_empty() {
        s = "(empty)".to_string();
    }
    text::normalize(&s, &ctx).into_owned()
}

/// Chunks (each a vec of `content_rows` lines) of a message at `cols`.
fn chunks(app: &App, m: &Message, cols: u16, content_rows: u16) -> Vec<Vec<String>> {
    let s = message_text(app, m);
    let lines: Vec<String> = text::wrap(&s, usize::from(cols.max(1)))
        .into_iter()
        .map(|l| l.text)
        .collect();
    let per = usize::from(content_rows.max(1));
    let mut out: Vec<Vec<String>> = lines.chunks(per).map(<[String]>::to_vec).collect();
    if out.is_empty() {
        out.push(vec![String::new()]);
    }
    out
}

fn msgs(app: &App) -> Vec<Message> {
    app.active_messages()
        .map(|m| m.iter().cloned().collect())
        .unwrap_or_default()
}

fn current_index(app: &App, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    Some(app.micro.msg.map_or(n - 1, |i| i.min(n - 1)))
}

/// Nav items at the current level (or filter results): `(label, unread badge, target)`.
fn nav_items(app: &App) -> Vec<(String, String, NavTarget)> {
    let g = app.glyphs();
    let ascii = g.is_ascii() || app.cfg.charset == bbs_core::ResolvedCharset::VtSafe;
    let sig = |k: ChannelKind| {
        if ascii {
            k.ascii_sigil()
        } else {
            k.utf8_sigil()
        }
    };
    let badge = |u: u32, m: u32| {
        if m > 0 {
            format!("{m}@")
        } else if u > 0 {
            format!("{}*", u.min(99))
        } else {
            String::new()
        }
    };
    let dm_label = |c: &bbs_core::Channel| {
        if c.kind.is_group_dm() && c.name.trim().is_empty() {
            let names: Vec<&str> = c
                .recipients
                .iter()
                .map(bbs_core::User::display_name)
                .collect();
            format!("&{}", names.join(","))
        } else {
            format!("{}{}", sig(c.kind), c.display_label())
        }
    };
    let mut out = Vec::new();
    if !app.micro.filter.is_empty() {
        use fuzzy_matcher::FuzzyMatcher;
        let m = fuzzy_matcher::skim::SkimMatcherV2::default();
        let mut scored: Vec<(i64, (String, String, NavTarget))> = Vec::new();
        for c in app.store.dms_by_recency() {
            let l = dm_label(c);
            if let Some(s) = m.fuzzy_match(&l, &app.micro.filter) {
                scored.push((
                    s,
                    (
                        l,
                        badge(c.unread_count, c.mention_count),
                        NavTarget::Channel(c.id),
                    ),
                ));
            }
        }
        for gd in app.store.ordered_guilds() {
            for c in app.store.sidebar_channels(gd.id) {
                if c.kind == ChannelKind::GuildCategory {
                    continue;
                }
                let l = format!("{}{}", sig(c.kind), c.name);
                if let Some(s) = m.fuzzy_match(&l, &app.micro.filter) {
                    scored.push((
                        s,
                        (
                            l,
                            badge(c.unread_count, c.mention_count),
                            NavTarget::Channel(c.id),
                        ),
                    ));
                }
            }
        }
        scored.sort_by_key(|x| std::cmp::Reverse(x.0));
        return scored.into_iter().map(|(_, x)| x).collect();
    }
    match app.micro.level {
        NavLevel::Root => {
            let (u, m) = app.store.dm_unread();
            out.push(("DMs".to_string(), badge(u, m), NavTarget::Level(None)));
            let mut guilds: Vec<&bbs_core::Guild> = app.store.ordered_guilds();
            guilds.sort_by_key(|g| (g.mention_count == 0, !g.unread));
            for gd in guilds {
                out.push((
                    sanitize_single_line(&gd.name).into_owned(),
                    badge(u32::from(gd.unread), gd.mention_count),
                    NavTarget::Level(Some(gd.id)),
                ));
            }
        }
        NavLevel::In(None) => {
            for c in app.store.dms_by_recency() {
                out.push((
                    dm_label(c),
                    badge(c.unread_count, c.mention_count),
                    NavTarget::Channel(c.id),
                ));
            }
        }
        NavLevel::In(Some(gid)) => {
            for c in app.store.sidebar_channels(gid) {
                if c.kind == ChannelKind::GuildCategory {
                    continue;
                }
                out.push((
                    format!("{}{}", sig(c.kind), sanitize_single_line(&c.name)),
                    badge(c.unread_count, c.mention_count),
                    NavTarget::Channel(c.id),
                ));
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavTarget {
    Level(Option<Snowflake>),
    Channel(Snowflake),
}

fn flags(app: &App) -> String {
    let active = app.active_channel;
    let mut mention = false;
    let mut unread = false;
    for c in app
        .store
        .dms
        .iter()
        .chain(app.store.channels.values().flatten())
    {
        if Some(c.id) == active {
            continue;
        }
        mention |= c.mention_count > 0;
        unread |= c.unread_count > 0;
    }
    let mut f = String::new();
    if mention {
        f.push('@');
    }
    if unread {
        f.push('*');
    }
    if let Some(v) = &app.store.voice {
        f.push(if v.self_deaf {
            'D'
        } else if v.self_mute {
            'M'
        } else {
            'V'
        });
    }
    f
}

/// Build the READ status row exactly `cols` wide (§3.2).
#[must_use]
pub fn status_row(author: &str, flags: &str, n: usize, m: usize, cols: u16) -> String {
    let cols = usize::from(cols);
    let fw = str_width(flags);
    let mut chunk = format!("{n}/{m}");
    let mut sep_flags = usize::from(fw > 0);
    let mut author_w = cols.saturating_sub(fw + sep_flags + 1 + str_width(&chunk));
    if author_w < 3 {
        chunk = format!("{n}");
        author_w = cols.saturating_sub(fw + sep_flags + 1 + str_width(&chunk));
    }
    if author_w < 3 {
        chunk.clear();
        author_w = cols.saturating_sub(fw + sep_flags);
    }
    if author_w < 3 {
        sep_flags = 0;
        author_w = cols.saturating_sub(fw);
    }
    let a = text::fit(author, author_w.min(cols));
    let mut s = format!(
        "{}{}{}",
        a.trim_end(),
        if sep_flags > 0 { " " } else { "" },
        flags
    );
    let left_w = str_width(&s);
    let cw = str_width(&chunk);
    if !chunk.is_empty() && left_w + 1 + cw <= cols {
        s.push_str(&" ".repeat(cols - left_w - cw));
        s.push_str(&chunk);
    }
    text::fit(&s, cols)
}

/// Advance auto-read (called from `App::tick`).
pub fn tick(app: &mut App) {
    if !app.micro.auto_read || app.micro.mode != MicroMode::Read || !app.is_micro() {
        return;
    }
    let (cols, rows) = app.size;
    let (_, content_rows, _) = rows_budget(rows);
    let all = msgs(app);
    let Some(i) = current_index(app, all.len()) else {
        return;
    };
    let ch = chunks(app, &all[i], cols, content_rows);
    let words = ch
        .get(app.micro.chunk)
        .map_or(1, |c| c.join(" ").split_whitespace().count().max(1));
    let dwell = ((60_000.0 * words as f64 / f64::from(app.cfg.wpm.max(60))) as u64).max(1200);
    if app.now_ms.saturating_sub(app.micro.last_advance_ms) >= dwell {
        app.micro.last_advance_ms = app.now_ms;
        step_chunk(app, 1);
    }
}

fn step_chunk(app: &mut App, dir: isize) {
    let (cols, rows) = app.size;
    let (_, content_rows, _) = rows_budget(rows);
    let all = msgs(app);
    let Some(i) = current_index(app, all.len()) else {
        return;
    };
    let n = chunks(app, &all[i], cols, content_rows).len();
    if dir > 0 {
        if app.micro.chunk + 1 < n {
            app.micro.chunk += 1;
        } else if i + 1 < all.len() {
            app.micro.msg = if i + 1 == all.len() - 1 {
                None
            } else {
                Some(i + 1)
            };
            app.micro.chunk = 0;
        }
    } else if app.micro.chunk > 0 {
        app.micro.chunk -= 1;
    } else if i > 0 {
        app.micro.msg = Some(i - 1);
        let prev_n = chunks(app, &all[i - 1], cols, content_rows).len();
        app.micro.chunk = prev_n.saturating_sub(1);
        if i - 1 == 0 {
            app.request_older();
        }
    }
}

fn step_msg(app: &mut App, dir: isize) {
    let all = msgs(app);
    let Some(i) = current_index(app, all.len()) else {
        return;
    };
    let next = (i as isize + dir).clamp(0, all.len() as isize - 1) as usize;
    app.micro.msg = if next == all.len() - 1 {
        None
    } else {
        Some(next)
    };
    app.micro.chunk = 0;
    if next == 0 {
        app.request_older();
    }
}

/// Handle a key in Micro mode.
pub fn handle_key(app: &mut App, k: KeyEvent) {
    let ctl = k.modifiers.contains(KeyModifiers::CONTROL);
    // Ring preempts everything except compose.
    if let CallRingState::IncomingRinging { .. } = app.store.ring
        && app.micro.mode != MicroMode::Compose
    {
        match k.code {
            KeyCode::Enter => return app.answer_or_hangup(),
            KeyCode::Left => return app.decline(),
            _ => {}
        }
    }
    let mode = app.micro.mode;
    match mode {
        MicroMode::Read => match k.code {
            KeyCode::Down => step_chunk(app, 1),
            KeyCode::Up => step_chunk(app, -1),
            KeyCode::Right => step_msg(app, 1),
            KeyCode::Left => step_msg(app, -1),
            KeyCode::Enter => {
                app.micro.mode = MicroMode::Msg;
                app.micro.menu = 0;
            }
            KeyCode::Esc => {
                app.micro.mode = MicroMode::Nav;
                app.micro.filter.clear();
            }
            KeyCode::Char(' ') => app.micro.auto_read = !app.micro.auto_read,
            KeyCode::Char('i') if !ctl => app.micro.enter_compose(),
            KeyCode::Char('r') if !ctl => {
                reply_current(app);
                app.micro.enter_compose();
            }
            KeyCode::Char('m') if !ctl => app.toggle_mute(),
            _ => {}
        },
        MicroMode::Nav => {
            let items = nav_items(app);
            match k.code {
                KeyCode::Up => app.micro.cursor = app.micro.cursor.saturating_sub(1),
                KeyCode::Down if !items.is_empty() => {
                    app.micro.cursor = (app.micro.cursor + 1).min(items.len() - 1);
                }
                KeyCode::Right | KeyCode::Enter => {
                    if let Some((_, _, t)) = items.get(app.micro.cursor).cloned() {
                        match t {
                            NavTarget::Level(g) => {
                                app.micro.level = NavLevel::In(g);
                                app.micro.cursor = 0;
                                app.select_guild(g);
                            }
                            NavTarget::Channel(id) => {
                                if k.code == KeyCode::Enter || app.micro.filter.is_empty() {
                                    let voice =
                                        app.store.channel(id).is_some_and(|c| c.kind.is_voice());
                                    app.select_channel(id);
                                    app.micro.filter.clear();
                                    app.micro.msg = None;
                                    app.micro.chunk = 0;
                                    app.micro.mode = if voice {
                                        MicroMode::Voice
                                    } else {
                                        MicroMode::Read
                                    };
                                }
                            }
                        }
                    }
                }
                KeyCode::Left => {
                    if !app.micro.filter.is_empty() {
                        app.micro.filter.clear();
                    } else {
                        app.micro.level = NavLevel::Root;
                    }
                    app.micro.cursor = 0;
                }
                KeyCode::Esc => {
                    if !app.micro.filter.is_empty() {
                        app.micro.filter.clear();
                    } else if app.micro.level != NavLevel::Root {
                        app.micro.level = NavLevel::Root;
                        app.micro.cursor = 0;
                    } else {
                        app.micro.mode = MicroMode::Main;
                        app.micro.menu = 0;
                    }
                }
                KeyCode::Backspace => {
                    app.micro.filter.pop();
                    app.micro.cursor = 0;
                }
                KeyCode::Char(c) if !ctl => {
                    app.micro.filter.push(c);
                    app.micro.cursor = 0;
                }
                _ => {}
            }
        }
        MicroMode::Main => match k.code {
            KeyCode::Up => app.micro.menu = app.micro.menu.saturating_sub(1),
            KeyCode::Down => app.micro.menu = (app.micro.menu + 1).min(MAIN_ITEMS.len() - 1),
            KeyCode::Esc | KeyCode::Left => app.micro.mode = MicroMode::Read,
            KeyCode::Enter | KeyCode::Right => match app.micro.menu {
                0 => app.micro.mode = MicroMode::Nav,
                1 => {
                    app.composer.target = ComposeTarget::New;
                    app.micro.enter_compose();
                }
                2 => app.micro.mode = MicroMode::Voice,
                3 => open_gif(app),
                4 => {
                    app.micro.filter.clear();
                    app.micro.mode = MicroMode::Search;
                }
                5 => {
                    app.micro.theme_prev = Some(app.theme().name().to_string());
                    app.rescan_themes();
                    let name = app.theme().name().to_string();
                    app.micro.theme = app.theme_catalog().position(&name).unwrap_or(0);
                    app.micro.theme_spec_prev = Some(app.theme().spec().clone());
                    app.micro.mode = MicroMode::Theme;
                }
                6 => {
                    app.micro.auto_read = !app.micro.auto_read;
                    app.micro.mode = MicroMode::Read;
                }
                _ => app.open(crate::modals::Modal::confirm_quit()),
            },
            _ => {}
        },
        MicroMode::Msg => match k.code {
            KeyCode::Up => app.micro.menu = app.micro.menu.saturating_sub(1),
            KeyCode::Down => app.micro.menu = (app.micro.menu + 1).min(MSG_ITEMS.len() - 1),
            KeyCode::Esc | KeyCode::Left => app.micro.mode = MicroMode::Read,
            KeyCode::Enter | KeyCode::Right => match app.micro.menu {
                0 => {
                    app.composer.target = ComposeTarget::New;
                    app.micro.enter_compose();
                }
                1 => {
                    reply_current(app);
                    app.micro.enter_compose();
                }
                2 => {
                    react_current(app, "👍");
                    app.micro.mode = MicroMode::Read;
                }
                3 => {
                    let all = msgs(app);
                    if let Some(i) = current_index(app, all.len()) {
                        app.selected = Some(all[i].id);
                        app.play_selected_voice();
                        app.selected = None;
                    }
                    app.micro.mode = MicroMode::Read;
                }
                4 => {
                    let ch = app.active_channel;
                    let thread = app
                        .store
                        .channels
                        .values()
                        .flatten()
                        .find(|t| t.kind.is_thread() && t.parent_id == ch)
                        .map(|t| t.id);
                    if let Some(t) = thread {
                        app.select_channel(t);
                        app.micro.msg = None;
                        app.micro.chunk = 0;
                    }
                    app.micro.mode = MicroMode::Read;
                }
                5 => open_gif(app),
                _ => {
                    app.micro.auto_read = !app.micro.auto_read;
                    app.micro.mode = MicroMode::Read;
                }
            },
            _ => {}
        },
        MicroMode::Compose => {
            if k.code == KeyCode::Esc {
                if app.composer.is_empty() || app.micro.esc_armed {
                    app.composer.clear();
                    app.micro.esc_armed = false;
                    app.micro.mode = MicroMode::Read;
                } else {
                    app.micro.esc_armed = true;
                }
                return;
            }
            app.micro.esc_armed = false;
            match k.code {
                KeyCode::Enter => {
                    app.send();
                    app.micro.msg = None;
                    app.micro.chunk = 0;
                    app.micro.mode = MicroMode::Read;
                }
                KeyCode::Backspace => app.composer.backspace(),
                KeyCode::Char('h') if ctl => app.composer.backspace(),
                KeyCode::Left => app.composer.left(),
                KeyCode::Right => app.composer.right(),
                KeyCode::Home => app.composer.home(),
                KeyCode::End => app.composer.end(),
                KeyCode::Char('u') if ctl => app.composer.kill_to_bol(),
                KeyCode::Char('w') if ctl => app.composer.kill_word_back(),
                KeyCode::Char(c) if !ctl => app.composer.insert_char(c),
                _ => {}
            }
        }
        MicroMode::Voice => {
            let n = app.store.voice.as_ref().map_or(0, |v| v.participants.len());
            match k.code {
                KeyCode::Up => app.micro.voice = app.micro.voice.saturating_sub(1),
                KeyCode::Down => app.micro.voice = (app.micro.voice + 1).min(n.saturating_sub(1)),
                KeyCode::Enter | KeyCode::Right => {
                    if app.store.voice.is_some() {
                        app.micro.mode = MicroMode::VoiceMenu;
                        app.micro.menu = 0;
                    } else {
                        app.micro.mode = MicroMode::Nav;
                    }
                }
                KeyCode::Esc | KeyCode::Left => app.micro.mode = MicroMode::Read,
                _ => {}
            }
        }
        MicroMode::VoiceMenu => match k.code {
            KeyCode::Up => app.micro.menu = app.micro.menu.saturating_sub(1),
            KeyCode::Down => app.micro.menu = (app.micro.menu + 1).min(VOICE_ITEMS.len() - 1),
            KeyCode::Esc | KeyCode::Left => app.micro.mode = MicroMode::Voice,
            KeyCode::Enter | KeyCode::Right => {
                app.deck_cursor = app.micro.voice;
                match app.micro.menu {
                    0 => app.toggle_mute(),
                    1 => app.toggle_deaf(),
                    2 => app.volume_step(10, false),
                    3 => app.volume_step(-10, false),
                    4 => {
                        if let Some(v) = &app.store.voice
                            && let Some(p) = v.participants.get(app.micro.voice)
                        {
                            let id = p.user_id;
                            if !app.dave_verified.remove(&id) {
                                app.dave_verified.insert(id);
                            }
                        }
                    }
                    _ => {
                        app.pending.push(Effect::Action(UiAction::LeaveVoice));
                        app.micro.mode = MicroMode::Read;
                        return;
                    }
                }
                app.micro.mode = MicroMode::Voice;
            }
            _ => {}
        },
        MicroMode::Gif => {
            let n = gif_items(app).len();
            match k.code {
                KeyCode::Left => app.micro.gif = app.micro.gif.saturating_sub(1),
                KeyCode::Right | KeyCode::Down => {
                    app.micro.gif = (app.micro.gif + 1).min(n.saturating_sub(1))
                }
                KeyCode::Up => app.micro.mode = MicroMode::GifQuery,
                KeyCode::Enter => {
                    if let (Some(item), Some(ch)) = (
                        gif_items(app).get(app.micro.gif).cloned(),
                        app.active_channel,
                    ) {
                        app.pending.push(Effect::Action(UiAction::SendGif {
                            channel_id: ch,
                            gif: item,
                            reply_to: None,
                        }));
                        app.micro.mode = MicroMode::Read;
                        app.micro.msg = None;
                    }
                }
                KeyCode::Esc => app.micro.mode = MicroMode::Read,
                KeyCode::Char(c) if !ctl => {
                    app.micro.filter.push(c);
                    app.micro.mode = MicroMode::GifQuery;
                }
                _ => {}
            }
        }
        MicroMode::GifQuery => match k.code {
            KeyCode::Enter | KeyCode::Down => {
                search_gif(app);
                app.micro.mode = MicroMode::Gif;
            }
            KeyCode::Backspace => {
                app.micro.filter.pop();
            }
            KeyCode::Esc => app.micro.mode = MicroMode::Gif,
            KeyCode::Char(c) if !ctl => app.micro.filter.push(c),
            _ => {}
        },
        MicroMode::Search => match k.code {
            KeyCode::Enter => {
                let needle = app.micro.filter.to_lowercase();
                let all = msgs(app);
                if let Some(i) = all
                    .iter()
                    .rposition(|m| m.content.to_lowercase().contains(&needle))
                {
                    app.micro.msg = Some(i);
                    app.micro.chunk = 0;
                }
                app.micro.filter.clear();
                app.micro.mode = MicroMode::Read;
            }
            KeyCode::Backspace => {
                app.micro.filter.pop();
            }
            KeyCode::Esc => {
                app.micro.filter.clear();
                app.micro.mode = MicroMode::Read;
            }
            KeyCode::Char(c) if !ctl => app.micro.filter.push(c),
            _ => {}
        },
        MicroMode::Theme => {
            let n = app.theme_catalog().len();
            match k.code {
                KeyCode::Left | KeyCode::Up => app.micro.theme = app.micro.theme.saturating_sub(1),
                KeyCode::Right | KeyCode::Down => {
                    app.micro.theme = (app.micro.theme + 1).min(n.saturating_sub(1))
                }
                KeyCode::Enter => {
                    app.cfg.theme = app.theme().name().to_string();
                    app.micro.theme_prev = None;
                    app.micro.theme_spec_prev = None;
                    app.micro.mode = MicroMode::Read;
                    return;
                }
                KeyCode::Esc => {
                    app.micro.theme_prev = None;
                    if let Some(prev) = app.micro.theme_spec_prev.take() {
                        app.set_theme(prev);
                    }
                    app.micro.mode = MicroMode::Read;
                    return;
                }
                _ => {}
            }
            let idx = app.micro.theme;
            app.apply_catalog(idx);
        }
    }
}

fn reply_current(app: &mut App) {
    let all = msgs(app);
    if let Some(i) = current_index(app, all.len()) {
        let m = &all[i];
        app.composer.target = ComposeTarget::Reply {
            message_id: m.id,
            author: m.author.display_name().to_string(),
            snippet: text::truncate(&sanitize_single_line(&m.content), 40, "."),
        };
    }
}

fn react_current(app: &mut App, emoji: &str) {
    let all = msgs(app);
    if let (Some(i), Some(ch)) = (current_index(app, all.len()), app.active_channel) {
        let id = all[i].id;
        app.pending.push(Effect::Action(UiAction::ToggleReaction {
            channel_id: ch,
            message_id: id,
            emoji: emoji.to_string(),
            add: true,
        }));
    }
}

fn open_gif(app: &mut App) {
    app.micro.filter.clear();
    app.micro.gif = 0;
    search_gif(app);
    app.micro.mode = MicroMode::Gif;
}

fn search_gif(app: &mut App) {
    app.gif_req = app.gif_req.wrapping_add(1);
    app.micro.gif_req = app.gif_req;
    app.micro.gif = 0;
    let q = bbs_core::GifSearchQuery {
        query: app.micro.filter.clone(),
        trending: app.micro.filter.is_empty(),
        ..bbs_core::GifSearchQuery::default()
    };
    app.pending.push(Effect::Action(UiAction::SearchGifs {
        request_id: app.gif_req,
        query: q,
    }));
}

fn gif_items(app: &App) -> Vec<bbs_core::GifItem> {
    app.store
        .gif_results
        .as_ref()
        .filter(|(id, _, _)| *id == app.micro.gif_req)
        .map(|(_, v, _)| v.clone())
        .unwrap_or_default()
}

/// Render the pager. Returns the cursor in compose modes.
pub fn render(app: &mut App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) -> Option<(u16, u16)> {
    let cols = area.width;
    let rows = area.height;
    if cols == 0 || rows == 0 {
        return None;
    }
    let base = ctx.base();
    let strong = ctx.s(Role::Title).add_modifier(Modifier::BOLD);
    // Floor: fewer than 8 columns → single status glyph.
    if cols < 8 {
        let mentions: u32 = app
            .store
            .dms
            .iter()
            .chain(app.store.channels.values().flatten())
            .map(|c| c.mention_count)
            .sum();
        let unread: u32 = app
            .store
            .dms
            .iter()
            .chain(app.store.channels.values().flatten())
            .map(|c| c.unread_count)
            .sum();
        let g = if mentions > 0 {
            "!".to_string()
        } else if unread > 9 {
            "+".to_string()
        } else if unread > 0 {
            unread.to_string()
        } else {
            ".".to_string()
        };
        draw::put(buf, area.x, area.y, &g, 1, strong);
        return None;
    }
    let (status, content_rows, hint) = rows_budget(rows);
    let line = |buf: &mut Buffer, r: u16, s: &str, st| {
        if r < rows {
            draw::put_fit(buf, area.x, area.y + r, &ctx.n(s), cols, st);
        }
    };
    let content_y = u16::from(status);
    let hint_y = rows - 1;

    // Ring preempts row 0.
    if let CallRingState::IncomingRinging { caller_name, .. } = &app.store.ring
        && app.micro.mode != MicroMode::Compose
    {
        let s = format!("RING {} E=ok", sanitize_single_line(caller_name));
        line(buf, 0, &s, strong);
        if rows == 1 {
            return None;
        }
    }
    let ringing = matches!(app.store.ring, CallRingState::IncomingRinging { .. })
        && app.micro.mode != MicroMode::Compose;
    let put_status = |buf: &mut Buffer, s: &str| {
        if status && !ringing {
            draw::put_fit(buf, area.x, area.y, &ctx.n(s), cols, strong);
        }
    };
    let list = |buf: &mut Buffer, items: &[String], cursor: usize| {
        let n = usize::from(content_rows);
        let start = cursor.saturating_sub(n.saturating_sub(1));
        for (i, it) in items.iter().enumerate().skip(start).take(n) {
            let marker = if i == cursor { ctx.g.cursor } else { " " };
            let marker = if ctx.g.is_ascii() || str_width(marker) != 1 {
                if i == cursor { ">" } else { " " }
            } else {
                marker
            };
            let s = format!("{marker}{it}");
            let st = if i == cursor {
                ctx.s(Role::Hotkey)
            } else {
                base
            };
            draw::put_fit(
                buf,
                area.x,
                area.y + content_y + u16::try_from(i - start).unwrap_or(0),
                &ctx.n(&s),
                cols,
                st,
            );
        }
    };

    match app.micro.mode {
        MicroMode::Read => {
            let all = msgs(app);
            let Some(i) = current_index(app, all.len()) else {
                put_status(buf, "bbscord");
                line(
                    buf,
                    content_y,
                    if app.active_channel.is_some() {
                        "(no messages)"
                    } else {
                        "Esc: channels"
                    },
                    base,
                );
                if hint && rows > 2 {
                    line(buf, hint_y, "ESC nav ENT menu", ctx.s(Role::Inactive));
                }
                return None;
            };
            let m = &all[i];
            let ch = chunks(app, m, cols, content_rows);
            app.micro.chunk = app.micro.chunk.min(ch.len() - 1);
            let author = text::normalize(&sanitize_single_line(m.author.display_name()), &ctx.text)
                .into_owned();
            let f = flags(app);
            put_status(
                buf,
                &status_row(&author, &f, app.micro.chunk + 1, ch.len(), cols),
            );
            if !status && !f.is_empty() {
                // rows == 1: flags as a ≤2-cell prefix.
                let pre: String = f.chars().take(2).collect();
                let body = ch[app.micro.chunk].first().cloned().unwrap_or_default();
                line(buf, 0, &format!("{pre}{body}"), base);
            } else {
                for (r, l) in ch[app.micro.chunk].iter().enumerate() {
                    line(buf, content_y + u16::try_from(r).unwrap_or(0), l, base);
                }
            }
            if hint && rows > 2 {
                let h = if app.micro.auto_read {
                    "AUTO ENT menu"
                } else {
                    "ENT menu ESC nav"
                };
                line(buf, hint_y, h, ctx.s(Role::Inactive));
            }
            None
        }
        MicroMode::Nav => {
            let items = nav_items(app);
            app.micro.cursor = app.micro.cursor.min(items.len().saturating_sub(1));
            let crumb = if !app.micro.filter.is_empty() {
                format!("/{}_ {}", app.micro.filter, items.len())
            } else {
                let lvl = match app.micro.level {
                    NavLevel::Root => "Home".to_string(),
                    NavLevel::In(None) => "DMs".to_string(),
                    NavLevel::In(Some(g)) => app
                        .store
                        .guild(g)
                        .map(|g| sanitize_single_line(&g.name).into_owned())
                        .unwrap_or_default(),
                };
                let pos = format!(
                    "{}/{}",
                    (app.micro.cursor + 1).min(items.len()),
                    items.len()
                );
                let w = usize::from(cols).saturating_sub(pos.len() + 1);
                format!("{} {pos}", text::fit(&format!("G:{lvl}"), w))
            };
            put_status(buf, &crumb);
            let labels: Vec<String> = items
                .iter()
                .map(|(l, b, _)| {
                    let w = usize::from(cols)
                        .saturating_sub(1 + str_width(b) + usize::from(!b.is_empty()));
                    let body = text::fit(&sanitize_single_line(l), w);
                    if b.is_empty() {
                        body
                    } else {
                        format!("{body} {b}")
                    }
                })
                .collect();
            if labels.is_empty() {
                line(buf, content_y, "(empty)", ctx.s(Role::Inactive));
            } else {
                list(buf, &labels, app.micro.cursor);
            }
            if hint && rows > 2 {
                line(buf, hint_y, "<> level ENT open", ctx.s(Role::Inactive));
            }
            None
        }
        MicroMode::Main | MicroMode::Msg | MicroMode::VoiceMenu => {
            let (title, items): (&str, &[&str]) = match app.micro.mode {
                MicroMode::Main => ("MAIN MENU", &MAIN_ITEMS),
                MicroMode::Msg => ("MSG MENU", &MSG_ITEMS),
                _ => ("VOICE", &VOICE_ITEMS),
            };
            put_status(
                buf,
                &format!("{title} {}/{}", app.micro.menu + 1, items.len()),
            );
            let labels: Vec<String> = items.iter().map(|s| (*s).to_string()).collect();
            list(buf, &labels, app.micro.menu);
            if hint && rows > 2 {
                line(buf, hint_y, "ENT ok ESC back", ctx.s(Role::Inactive));
            }
            None
        }
        MicroMode::Compose | MicroMode::GifQuery | MicroMode::Search => {
            let (head, input_y) = match app.micro.mode {
                MicroMode::Compose => {
                    let target = match &app.composer.target {
                        ComposeTarget::Reply { author, .. } => {
                            format!(">{}", sanitize_single_line(author))
                        }
                        ComposeTarget::Edit { .. } => ">edit".to_string(),
                        ComposeTarget::New => {
                            let c = app
                                .active_channel
                                .and_then(|c| app.store.channel(c))
                                .map(|c| c.display_label())
                                .unwrap_or_default();
                            format!(">{}", sanitize_single_line(&c))
                        }
                    };
                    let max = app.cfg.max_message_len();
                    let counter = format!("{}/{}k", app.composer.char_len(), max / 1000);
                    let head = if app.micro.esc_armed {
                        "Esc=drop".to_string()
                    } else {
                        let w = usize::from(cols).saturating_sub(counter.len() + 1);
                        format!("{} {counter}", text::fit(&target, w))
                    };
                    (head, if rows >= 3 { hint_y } else { content_y })
                }
                MicroMode::GifQuery => ("GIF search:".to_string(), content_y),
                _ => ("Find:".to_string(), content_y),
            };
            put_status(buf, &head);
            if app.micro.mode == MicroMode::Compose {
                let (vis, col) = app.composer.micro_view(cols);
                let vis = ctx.n(&vis);
                draw::put_fit(buf, area.x, area.y + input_y, &vis, cols, base);
                Some((area.x + col.min(cols - 1), area.y + input_y))
            } else {
                let q = ctx.n(&app.micro.filter);
                let shown = text::truncate(&q, usize::from(cols - 1), "");
                let w = u16::try_from(str_width(&shown)).unwrap_or(0);
                draw::put_fit(buf, area.x, area.y + input_y, &shown, cols, base);
                Some((area.x + w.min(cols - 1), area.y + input_y))
            }
        }
        MicroMode::Voice => {
            let Some(v) = app.store.voice.clone() else {
                put_status(buf, "VOICE");
                line(buf, content_y, "not in voice", base);
                if hint && rows > 2 {
                    line(buf, hint_y, "ENT pick channel", ctx.s(Role::Inactive));
                }
                return None;
            };
            let flag = if v.self_deaf {
                " D"
            } else if v.self_mute {
                " M"
            } else {
                ""
            };
            let name = sanitize_single_line(&v.channel_name);
            let tail = format!(" {}u{flag}", v.participants.len());
            let w = usize::from(cols).saturating_sub(3 + tail.len());
            put_status(buf, &format!("VC:{}{tail}", text::fit(&name, w).trim_end()));
            let rows_n = usize::from(content_rows);
            app.micro.voice = app.micro.voice.min(v.participants.len().saturating_sub(1));
            let start = app.micro.voice.saturating_sub(rows_n.saturating_sub(1));
            for (i, p) in v.participants.iter().enumerate().skip(start).take(rows_n) {
                let vu_w = usize::from(cols).saturating_sub(2).clamp(2, 8) - 2;
                let name_w = usize::from(cols).saturating_sub(vu_w + 3);
                let cells: String =
                    crate::meters::vu_cells(p.normalized_vu(), vu_w, ctx.cfg.charset)
                        .into_iter()
                        .collect();
                let marker = if i == app.micro.voice { ">" } else { " " };
                let s = format!(
                    "{marker}{} [{cells}]",
                    text::fit(
                        &sanitize_single_line(&p.display_name),
                        name_w.saturating_sub(1)
                    )
                );
                line(
                    buf,
                    content_y + u16::try_from(i - start).unwrap_or(0),
                    &s,
                    if p.speaking {
                        ctx.s(Role::Hotkey)
                    } else {
                        base
                    },
                );
            }
            if hint && rows > 2 {
                line(buf, hint_y, "ENT actions", ctx.s(Role::Inactive));
            }
            None
        }
        MicroMode::Gif => {
            let items = gif_items(app);
            let q = if app.micro.filter.is_empty() {
                "trend".to_string()
            } else {
                app.micro.filter.clone()
            };
            let pos = format!(
                "{}/{}",
                if items.is_empty() {
                    0
                } else {
                    app.micro.gif + 1
                },
                items.len()
            );
            let w = usize::from(cols).saturating_sub(4 + pos.len() + 1);
            put_status(buf, &format!("GIF:{} {pos}", text::fit(&q, w)));
            let body = items.get(app.micro.gif).map_or_else(
                || "searching...".to_string(),
                |it| format!("[{}]", sanitize_single_line(&it.title)),
            );
            line(buf, content_y, &body, base);
            if hint && rows > 2 {
                line(buf, hint_y, "<> pick ENT send", ctx.s(Role::Inactive));
            }
            None
        }
        MicroMode::Theme => {
            put_status(buf, "THEME <> ENT");
            let name = app
                .theme_catalog()
                .entries
                .get(app.micro.theme)
                .map(|e| e.label.clone())
                .unwrap_or_default();
            line(buf, content_y, &name, ctx.s(Role::Hotkey));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_row_fits_16() {
        let s = status_row("bitloop", "@*M", 125, 125, 16);
        assert_eq!(str_width(&s), 16);
        assert!(s.ends_with("125/125"));
        assert!(s.starts_with("bitl"));
        let s = status_row("ali", "", 3, 9, 16);
        assert_eq!(s, "ali          3/9");
        let s = status_row("someone", "@*M", 1000, 1000, 8);
        assert_eq!(str_width(&s), 8);
    }

    #[test]
    fn budgets() {
        assert_eq!(rows_budget(2), (true, 1, false));
        assert_eq!(rows_budget(4), (true, 2, true));
        assert_eq!(rows_budget(10), (true, 8, true));
    }
}
