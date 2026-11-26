//! Composer: a small readline-flavoured multi-line editor (§9.2 INSERT tier).

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{grapheme_width, str_width};

/// What the composer is doing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ComposeTarget {
    /// New message.
    #[default]
    New,
    /// Reply to a message.
    Reply {
        /// Target message.
        message_id: bbs_core::Snowflake,
        /// Author display name.
        author: String,
        /// One-line snippet.
        snippet: String,
        /// Whether sending this reply pings (`@mentions`) the replied-to author.
        mention: bool,
    },
    /// Editing an own message.
    Edit {
        /// Target message.
        message_id: bbs_core::Snowflake,
    },
}

/// Editor buffer. Cursor is a byte offset on a grapheme boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    text: String,
    cursor: usize,
    kill_ring: String,
    /// Reply/edit target.
    pub target: ComposeTarget,
    /// Staged attachment paths.
    pub attachments: Vec<String>,
    /// Micro horizontal scroll (grapheme index of first visible cluster).
    pub micro_scroll: usize,
}

impl Composer {
    /// Current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Cursor byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Cursor at end of buffer.
    #[must_use]
    pub fn at_end(&self) -> bool {
        self.cursor >= self.text.len()
    }

    /// Length in chars (Discord counts UTF-16-ish; chars is a close, safe bound).
    #[must_use]
    pub fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    /// Replace the whole buffer.
    pub fn set_text(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    /// Clear text, target and attachments.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.target = ComposeTarget::New;
        self.attachments.clear();
        self.micro_scroll = 0;
    }

    /// Take the text, leaving the composer empty.
    pub fn take(&mut self) -> String {
        let t = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.micro_scroll = 0;
        t
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.text[..from]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..]
            .graphemes(true)
            .next()
            .map_or(self.text.len(), |g| from + g.len())
    }

    /// Insert text at the cursor.
    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Insert one char.
    pub fn insert_char(&mut self, c: char) {
        let mut b = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut b));
    }

    /// Replace a byte range `[start..end]` with `replacement`, moving cursor to the end of the
    /// inserted text. Silently no-ops if bounds are invalid or not on UTF-8 char boundaries.
    pub fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        if start <= end
            && end <= self.text.len()
            && self.text.is_char_boundary(start)
            && self.text.is_char_boundary(end)
        {
            self.text.replace_range(start..end, replacement);
            self.cursor = start + replacement.len();
        }
    }

    /// Delete the grapheme before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let p = self.prev_boundary(self.cursor);
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    /// Delete the grapheme under the cursor.
    pub fn delete(&mut self) {
        if self.at_end() {
            return;
        }
        let n = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..n, "");
    }

    /// Move left one grapheme.
    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
    }

    /// Move right one grapheme.
    pub fn right(&mut self) {
        if !self.at_end() {
            self.cursor = self.next_boundary(self.cursor);
        }
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }

    /// Readline `Ctrl+A`.
    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    /// Readline `Ctrl+E`.
    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    /// Readline `Alt+B`.
    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        let start = trimmed
            .rfind(|c: char| !c.is_alphanumeric())
            .map_or(0, |i| {
                i + trimmed[i..].chars().next().map_or(1, char::len_utf8)
            });
        self.cursor = start;
    }

    /// Readline `Alt+F`.
    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skip_ws = after.len()
            - after
                .trim_start_matches(|c: char| !c.is_alphanumeric())
                .len();
        let rest = &after[skip_ws..];
        let word = rest
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(rest.len());
        self.cursor += skip_ws + word;
    }

    /// Readline `Ctrl+K`; returns false if nothing was killed.
    pub fn kill_to_eol(&mut self) -> bool {
        let e = self.line_end();
        if e == self.cursor {
            // At end of a non-final line: join (emacs semantics).
            if self.cursor < self.text.len() {
                self.kill_ring = "\n".to_string();
                self.text.remove(self.cursor);
                return true;
            }
            return false;
        }
        self.kill_ring = self.text[self.cursor..e].to_string();
        self.text.replace_range(self.cursor..e, "");
        true
    }

    /// Readline `Ctrl+U`.
    pub fn kill_to_bol(&mut self) {
        let s = self.line_start();
        self.kill_ring = self.text[s..self.cursor].to_string();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
    }

    /// Readline `Ctrl+W`.
    pub fn kill_word_back(&mut self) {
        let end = self.cursor;
        let before = &self.text[..end];
        let trimmed = before.trim_end();
        let start = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
        self.kill_ring = self.text[start..end].to_string();
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Readline `Alt+D`.
    pub fn kill_word_fwd(&mut self) {
        let start = self.cursor;
        self.word_right();
        let end = self.cursor;
        self.kill_ring = self.text[start..end].to_string();
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    /// Readline `Ctrl+Y`.
    pub fn yank(&mut self) {
        let k = self.kill_ring.clone();
        self.insert_str(&k);
    }

    /// Readline `Ctrl+T`.
    pub fn transpose(&mut self) {
        if self.cursor == 0 || self.text.graphemes(true).count() < 2 {
            return;
        }
        if self.at_end() {
            self.left();
        }
        let a0 = self.prev_boundary(self.cursor);
        let b1 = self.next_boundary(self.cursor);
        let a = self.text[a0..self.cursor].to_string();
        let b = self.text[self.cursor..b1].to_string();
        self.text.replace_range(a0..b1, &format!("{b}{a}"));
        self.cursor = b1;
    }

    /// Wrap for display at `width` cells; returns `(lines, cursor_row, cursor_col)`.
    /// The last cell of a line is reserved for the cursor at EOL (§3.4).
    #[must_use]
    pub fn layout(&self, width: u16) -> (Vec<String>, u16, u16) {
        let width = usize::from(width.max(2));
        let mut lines = vec![String::new()];
        let mut col = 0usize;
        let (mut crow, mut ccol) = (0usize, 0usize);
        let mut byte = 0usize;
        for g in self.text.graphemes(true) {
            if byte == self.cursor {
                crow = lines.len() - 1;
                ccol = col;
            }
            byte += g.len();
            if g == "\n" {
                lines.push(String::new());
                col = 0;
                continue;
            }
            let w = grapheme_width(g);
            if col + w > width - 1 {
                lines.push(String::new());
                col = 0;
            }
            if let Some(l) = lines.last_mut() {
                l.push_str(g);
            }
            col += w;
        }
        if self.cursor >= self.text.len() {
            crow = lines.len() - 1;
            ccol = col;
        }
        let to16 = |v: usize| u16::try_from(v).unwrap_or(u16::MAX);
        (lines, to16(crow), to16(ccol))
    }

    /// Single-line horizontally scrolled view for Micro (§3.4): returns `(visible, cursor_col)`.
    pub fn micro_view(&mut self, width: u16) -> (String, u16) {
        let width = usize::from(width.max(1));
        let flat: String = self.text.replace('\n', " ");
        let gs: Vec<&str> = flat.graphemes(true).collect();
        let cursor_g = self.text[..self.cursor].graphemes(true).count();
        if cursor_g < self.micro_scroll {
            self.micro_scroll = cursor_g.saturating_sub(4);
        }
        loop {
            let used: usize = gs[self.micro_scroll.min(gs.len())..cursor_g.min(gs.len())]
                .iter()
                .map(|g| grapheme_width(g))
                .sum();
            if used < width || self.micro_scroll >= cursor_g {
                break;
            }
            self.micro_scroll += 1;
        }
        // Keep ≥4 cells of left context when scrolled.
        while self.micro_scroll > 0 {
            let ctx: usize = gs[self.micro_scroll..cursor_g.min(gs.len())]
                .iter()
                .map(|g| grapheme_width(g))
                .sum();
            if ctx >= 4 || ctx + grapheme_width(gs[self.micro_scroll - 1]) > width - 1 {
                break;
            }
            self.micro_scroll -= 1;
        }
        let mut out = String::new();
        let mut w = 0;
        let mut cursor_col = 0;
        for (i, g) in gs.iter().enumerate().skip(self.micro_scroll) {
            if i == cursor_g {
                cursor_col = w;
            }
            let gw = grapheme_width(g);
            if w + gw > width {
                break;
            }
            out.push_str(g);
            w += gw;
        }
        if cursor_g >= gs.len() {
            cursor_col = str_width(&out).min(width - 1);
        }
        (out, u16::try_from(cursor_col).unwrap_or(0))
    }
}

/// Autocomplete trigger category in the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// `@user` mention completion.
    Mention,
    /// `:shortcode:` emoji completion.
    Emoji,
    /// `#channel` completion.
    Channel,
}

/// One candidate item in the composer autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Primary display label shown in the popup.
    pub label: String,
    /// Secondary hint/detail shown to the right.
    pub detail: String,
    /// Literal text inserted when accepted.
    pub insert: String,
    /// Optional Snowflake ID (user or channel).
    pub id: Option<bbs_core::Snowflake>,
}

/// Active composer autocomplete popup state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutocompleteState {
    /// Byte offset of the trigger sigil (`@`, `:`, `#`) in the composer text.
    pub trigger_start: usize,
    /// Byte offset of the composer cursor when computed.
    pub trigger_end: usize,
    /// Active trigger category (`None` when closed).
    pub kind: Option<CompletionKind>,
    /// Filtered candidate items (up to 8).
    pub items: Vec<CompletionItem>,
    /// Highlighted row index in `items`.
    pub cursor: usize,
}

/// Built-in common Discord emoji shortcodes `(shortcode, unicode)`.
pub const COMMON_EMOJIS: &[(&str, &str)] = &[
    ("thumbsup", "👍"),
    ("+1", "👍"),
    ("thumbsdown", "👎"),
    ("-1", "👎"),
    ("heart", "❤️"),
    ("fire", "🔥"),
    ("joy", "😂"),
    ("rofl", "🤣"),
    ("sob", "😭"),
    ("smile", "😄"),
    ("grin", "😁"),
    ("wink", "😉"),
    ("blush", "😊"),
    ("sweat_smile", "😅"),
    ("sunglasses", "😎"),
    ("thinking", "🤔"),
    ("eyes", "👀"),
    ("tada", "🎉"),
    ("party", "🥳"),
    ("rocket", "🚀"),
    ("100", "💯"),
    ("sparkles", "✨"),
    ("star", "⭐"),
    ("wave", "👋"),
    ("clap", "👏"),
    ("pray", "🙏"),
    ("ok_hand", "👌"),
    ("raised_hands", "🙌"),
    ("muscle", "💪"),
    ("salute", "🫡"),
    ("shrug", "🤷"),
    ("skull", "💀"),
    ("ghost", "👻"),
    ("alien", "👽"),
    ("robot", "🤖"),
    ("poop", "💩"),
    ("check", "✅"),
    ("white_check_mark", "✅"),
    ("x", "❌"),
    ("warning", "⚠️"),
    ("question", "❓"),
    ("exclamation", "❗"),
    ("bulb", "💡"),
    ("zap", "⚡"),
    ("rainbow", "🌈"),
    ("sun", "☀️"),
    ("moon", "🌙"),
    ("snowflake", "❄️"),
    ("coffee", "☕"),
    ("pizza", "🍕"),
    ("beer", "🍺"),
    ("bug", "🐛"),
    ("wrench", "🔧"),
    ("hammer", "🔨"),
    ("gear", "⚙️"),
    ("lock", "🔒"),
    ("key", "🔑"),
    ("bell", "🔔"),
    ("mute", "🔇"),
    ("microphone", "🎤"),
    ("headphones", "🎧"),
    ("speaker", "🔊"),
    ("computer", "💻"),
    ("memo", "📝"),
    ("pin", "📌"),
    ("link", "🔗"),
    ("trash", "🗑️"),
    ("crown", "👑"),
    ("gem", "💎"),
    ("trophy", "🏆"),
    ("dart", "🎯"),
    ("gamepad", "🎮"),
    ("music", "🎵"),
    ("art", "🎨"),
    ("crab", "🦀"),
    ("cat", "🐱"),
    ("dog", "🐶"),
    ("fox", "🦊"),
    ("penguin", "🐧"),
    ("angry", "😠"),
    ("cry", "😢"),
    ("pleading", "🥺"),
    ("mind_blown", "🤯"),
    ("melting", "🫠"),
];

impl AutocompleteState {
    /// Whether the popup has active items to show.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.kind.is_some() && !self.items.is_empty()
    }

    /// Close and clear all items.
    pub fn clear(&mut self) {
        self.kind = None;
        self.items.clear();
        self.cursor = 0;
        self.trigger_start = 0;
        self.trigger_end = 0;
    }

    /// Move selection up (wrapping).
    pub fn move_up(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.cursor = if self.cursor == 0 {
            self.items.len() - 1
        } else {
            self.cursor - 1
        };
    }

    /// Move selection down (wrapping).
    pub fn move_down(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.items.len();
    }

    /// Accept the currently selected completion item into `composer`.
    pub fn accept(&mut self, composer: &mut Composer) -> bool {
        let Some(item) = self.items.get(self.cursor).cloned() else {
            self.clear();
            return false;
        };
        composer.replace_range(self.trigger_start, self.trigger_end, &item.insert);
        self.clear();
        true
    }

    /// Accept item at `index` (used by mouse click).
    pub fn accept_index(&mut self, index: usize, composer: &mut Composer) -> bool {
        if index >= self.items.len() {
            return false;
        }
        self.cursor = index;
        self.accept(composer)
    }

    /// Recompute autocomplete candidates from the current composer cursor position.
    pub fn refresh(
        &mut self,
        composer: &Composer,
        store: &crate::state::Store,
        active_channel: Option<bbs_core::Snowflake>,
        active_guild: Option<bbs_core::Snowflake>,
    ) {
        let text = composer.text();
        let cursor = composer.cursor().min(text.len());
        if !text.is_char_boundary(cursor) {
            self.clear();
            return;
        }
        let prefix = &text[..cursor];
        let token_start = prefix
            .rfind(|c: char| c.is_whitespace())
            .map_or(0, |i| i + prefix[i..].chars().next().map_or(1, char::len_utf8));
        let token = &prefix[token_start..];
        let Some(first) = token.chars().next() else {
            self.clear();
            return;
        };
        let query = &token[first.len_utf8()..];
        let ql = query.to_ascii_lowercase();

        let (kind, mut items) = match first {
            '@' => {
                let mut seen = std::collections::HashSet::new();
                let mut users: Vec<(&bbs_core::User, Option<&str>)> = Vec::new();
                // 1. DM recipients of active channel.
                if let Some(cid) = active_channel
                    && let Some(ch) = store.channel(cid)
                {
                    for r in &ch.recipients {
                        if seen.insert(r.id) {
                            users.push((r, None));
                        }
                    }
                }
                // 2. Guild members of active guild.
                if let Some(gid) = active_guild
                    && let Some(gm) = store.guild_members.get(&gid)
                {
                    for (uid, ms) in gm {
                        if let Some(u) = store.users.get(uid)
                            && seen.insert(u.id)
                        {
                            users.push((u, ms.nick.as_deref()));
                        }
                    }
                }
                // 3. Recent channel authors.
                if let Some(cid) = active_channel
                    && let Some(cm) = store.messages.get(&cid)
                {
                    for m in cm.messages.iter().rev().take(60) {
                        if seen.insert(m.author.id) {
                            let nick = active_guild
                                .and_then(|gid| store.member_nick(gid, m.author.id))
                                .or(m.author_nick.as_deref());
                            users.push((&m.author, nick));
                        }
                    }
                }
                // 4. All other known users.
                for u in store.users.values() {
                    if seen.insert(u.id) {
                        let nick = active_guild.and_then(|gid| store.member_nick(gid, u.id));
                        users.push((u, nick));
                    }
                }

                let mut scored: Vec<(u8, String, CompletionItem)> = Vec::new();
                for (u, nick) in users {
                    let display = nick
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| u.display_name());
                    let uname_l = u.username.to_ascii_lowercase();
                    let disp_l = display.to_ascii_lowercase();
                    let rank = if ql.is_empty() {
                        1
                    } else if uname_l.starts_with(&ql) || disp_l.starts_with(&ql) {
                        0
                    } else if uname_l.contains(&ql) || disp_l.contains(&ql) {
                        2
                    } else {
                        continue;
                    };
                    let insert = if !u.username.is_empty()
                        && !u.username.chars().any(char::is_whitespace)
                    {
                        format!("@{} ", u.username)
                    } else {
                        format!("<@{}> ", u.id)
                    };
                    let detail = if display != u.username {
                        format!("@{}", u.username)
                    } else {
                        String::new()
                    };
                    scored.push((
                        rank,
                        disp_l,
                        CompletionItem {
                            label: format!("@{display}"),
                            detail,
                            insert,
                            id: Some(u.id),
                        },
                    ));
                }
                scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                let items: Vec<CompletionItem> =
                    scored.into_iter().take(8).map(|(_, _, it)| it).collect();
                (CompletionKind::Mention, items)
            }
            ':' => {
                if query.is_empty() || query.contains(':') {
                    self.clear();
                    return;
                }
                let mut scored: Vec<(u8, &str, CompletionItem)> = Vec::new();
                for &(name, glyph) in COMMON_EMOJIS {
                    let rank = if name.starts_with(&ql) {
                        0
                    } else if name.contains(&ql) {
                        1
                    } else {
                        continue;
                    };
                    scored.push((
                        rank,
                        name,
                        CompletionItem {
                            label: format!("{glyph} :{name}:"),
                            detail: String::new(),
                            insert: format!("{glyph} "),
                            id: None,
                        },
                    ));
                }
                scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
                let items = scored.into_iter().take(8).map(|(_, _, it)| it).collect();
                (CompletionKind::Emoji, items)
            }
            '#' => {
                let Some(gid) = active_guild else {
                    self.clear();
                    return;
                };
                let Some(chs) = store.channels.get(&gid) else {
                    self.clear();
                    return;
                };
                let mut scored: Vec<(u8, String, CompletionItem)> = Vec::new();
                for c in chs {
                    if c.kind == bbs_core::ChannelKind::GuildCategory {
                        continue;
                    }
                    let nl = c.name.to_ascii_lowercase();
                    let rank = if ql.is_empty() {
                        1
                    } else if nl.starts_with(&ql) {
                        0
                    } else if nl.contains(&ql) {
                        2
                    } else {
                        continue;
                    };
                    let insert = if !c.name.is_empty() && !c.name.chars().any(char::is_whitespace) {
                        format!("#{} ", c.name)
                    } else {
                        format!("<#{}> ", c.id)
                    };
                    scored.push((
                        rank,
                        nl,
                        CompletionItem {
                            label: format!("#{}", c.name),
                            detail: c.topic.clone().unwrap_or_default(),
                            insert,
                            id: Some(c.id),
                        },
                    ));
                }
                scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                let items = scored.into_iter().take(8).map(|(_, _, it)| it).collect();
                (CompletionKind::Channel, items)
            }
            _ => {
                self.clear();
                return;
            }
        };

        if items.is_empty() {
            self.clear();
            return;
        }
        items.truncate(8);
        self.trigger_start = token_start;
        self.trigger_end = cursor;
        self.kind = Some(kind);
        self.items = items;
        if self.cursor >= self.items.len() {
            self.cursor = 0;
        }
    }
}

/// Expand human-readable `@username` and `#channel-name` tokens into Discord `<@id>` and `<#id>`
/// wire syntax outside backtick code spans and fenced code blocks.
#[must_use]
pub fn expand_outgoing_mentions(
    text: &str,
    store: &crate::state::Store,
    active_guild: Option<bbs_core::Snowflake>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut in_fence = false;
    let mut in_inline_code = false;

    while i < bytes.len() {
        if text[i..].starts_with("```") {
            in_fence = !in_fence;
            out.push_str("```");
            i += 3;
            continue;
        }
        if !in_fence && bytes[i] == b'`' {
            in_inline_code = !in_inline_code;
            out.push('`');
            i += 1;
            continue;
        }
        if !in_fence && !in_inline_code && (bytes[i] == b'@' || bytes[i] == b'#') {
            let prev_ok = i == 0 || {
                let prev = text[..i].chars().next_back();
                prev.is_some_and(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | ',' | ';'))
            };
            if prev_ok {
                let sigil = bytes[i];
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() {
                    let b = bytes[end];
                    let ok = if sigil == b'@' {
                        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
                    } else {
                        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
                    };
                    if ok {
                        end += 1;
                    } else {
                        break;
                    }
                }
                let word = &text[start..end];
                if !word.is_empty() {
                    if sigil == b'@'
                        && !word.eq_ignore_ascii_case("everyone")
                        && !word.eq_ignore_ascii_case("here")
                    {
                        if let Some(uid) = find_user_by_token(word, store, active_guild) {
                            out.push_str(&format!("<@{uid}>"));
                            i = end;
                            continue;
                        }
                    } else if sigil == b'#'
                        && let Some(gid) = active_guild
                        && let Some(chs) = store.channels.get(&gid)
                        && let Some(ch) = chs.iter().find(|c| {
                            c.kind != bbs_core::ChannelKind::GuildCategory
                                && c.name.eq_ignore_ascii_case(word)
                        })
                    {
                        out.push_str(&format!("<#{}>", ch.id));
                        i = end;
                        continue;
                    }
                }
            }
        }
        if let Some(ch) = text[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn find_user_by_token(
    word: &str,
    store: &crate::state::Store,
    active_guild: Option<bbs_core::Snowflake>,
) -> Option<bbs_core::Snowflake> {
    if let Some(u) = store
        .users
        .values()
        .find(|u| u.username.eq_ignore_ascii_case(word))
    {
        return Some(u.id);
    }
    if let Some(gid) = active_guild
        && let Some(gm) = store.guild_members.get(&gid)
        && let Some((uid, _)) = gm.iter().find(|(_, ms)| {
            ms.nick
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(word))
        })
    {
        return Some(*uid);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readline_ops() {
        let mut c = Composer::default();
        c.insert_str("hello world");
        c.kill_word_back();
        assert_eq!(c.text(), "hello ");
        c.yank();
        assert_eq!(c.text(), "hello world");
        c.home();
        assert!(c.kill_to_eol());
        assert_eq!(c.text(), "");
        assert!(!c.kill_to_eol());
        c.insert_str("ab");
        c.transpose();
        assert_eq!(c.text(), "ba");
    }

    #[test]
    fn micro_view_reserves_last_cell() {
        let mut c = Composer::default();
        c.insert_str("abcdefghijklmnopqrstuvwxyz");
        let (v, col) = c.micro_view(16);
        assert!(str_width(&v) <= 16);
        assert_eq!(col, 15);
        assert!(v.ends_with('z'));
    }

    #[test]
    fn layout_wraps_with_cursor() {
        let mut c = Composer::default();
        c.insert_str("abcdefgh");
        let (lines, r, col) = c.layout(5);
        assert_eq!(lines, vec!["abcd", "efgh"]);
        assert_eq!((r, col), (1, 4));
    }
}
