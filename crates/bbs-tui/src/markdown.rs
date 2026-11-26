//! Discord-flavoured Markdown → styled spans, plus span-aware wrapping.
//!
//! Block level (per `\n`-separated line): fenced code blocks (```` ```lang ````, highlighted with
//! syntect), `> ` / `>>> ` quotes, `#`/`##`/`###` headings, `-# ` subtext, `- ` / `* ` / `1. `
//! list items. Inline (hand-written, modelled on Discord's simple-markdown rules): `**bold**`,
//! `*italic*` / `_italic_`, `__underline__`, `~~strike~~`, `||spoiler||`, `` `code` ``,
//! `[masked](https://links)`, bare URLs, `<https://suppressed>`, mentions `<@id>` `<@!id>`
//! `<@&id>` `<#id>`, `@everyone`/`@here`, custom emoji `<:name:id>` / `<a:name:id>` → `:name:`,
//! timestamps `<t:unix:F>` (rendered in UTC, deterministic), slash commands `</name:id>`,
//! and backslash escapes.
//!
//! All span text is normalized with [`crate::text::normalize`] for the active charset and emoji
//! policy; link targets pass [`crate::sanitize::sanitize_osc8_url`].

use std::sync::OnceLock;

use bbs_core::RgbColor;
use unicode_segmentation::UnicodeSegmentation;

use crate::sanitize::{sanitize_osc8_url, sanitize_single_line};
use crate::text::{TextCtx, grapheme_width, normalize, wrap_ranges};

/// Mention kinds resolved through the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    /// `<@id>` / `<@!id>`.
    User,
    /// `<@&id>`.
    Role,
    /// `<#id>`.
    Channel,
}

/// Inline style flags for a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MdStyle {
    /// `**bold**`.
    pub bold: bool,
    /// `*italic*` / `_italic_`.
    pub italic: bool,
    /// `__underline__`.
    pub underline: bool,
    /// `~~strike~~`.
    pub strike: bool,
    /// `` `code` `` or code-block text.
    pub code: bool,
    /// `||spoiler||`.
    pub spoiler: bool,
    /// Link text.
    pub link: bool,
    /// Mention / channel pill.
    pub mention: bool,
    /// `-# subtext` (dim).
    pub subtext: bool,
    /// Explicit foreground (syntect highlighting).
    pub fg: Option<RgbColor>,
}

/// Styled text run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdSpan {
    /// Text (already normalized for the charset).
    pub text: String,
    /// Style.
    pub style: MdStyle,
    /// Link target (sanitized) if any.
    pub url: Option<String>,
}

/// Logical line kind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LineKind {
    /// Paragraph text.
    #[default]
    Normal,
    /// `> quote` / `>>> quote`. The quote bar is *not* part of the spans; the caller draws it.
    Quote,
    /// Fenced code block line.
    Code {
        /// Language tag.
        lang: Option<String>,
    },
    /// `#`, `##`, `###` heading (marker stripped).
    Heading(u8),
    /// List item (depth from 0). The indentation and bullet (`• ` / `1. `) *are* part of the spans.
    ListItem(u8),
}

/// Logical (unwrapped) line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MdLine {
    /// Kind.
    pub kind: LineKind,
    /// Spans.
    pub spans: Vec<MdSpan>,
}

/// Wrapped visual line of spans.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StyledLine {
    /// Kind inherited from the logical line.
    pub kind: LineKind,
    /// Spans on this visual line.
    pub spans: Vec<MdSpan>,
    /// Grapheme index of the first cluster in the rendered text of the whole message
    /// (logical lines joined by one `\n` grapheme each).
    pub start_grapheme: u32,
    /// Display width.
    pub width: u16,
}

/// Mention resolver signature: `(kind, id) → display name` (without or with sigil).
type Resolver<'a> = &'a dyn Fn(MentionKind, u64) -> Option<String>;

// ---------------------------------------------------------------------------------------------
// Syntax highlighting (lazy, bundled defaults)
// ---------------------------------------------------------------------------------------------

/// Code blocks larger than this (bytes) are not highlighted (bounded CPU per message).
const HIGHLIGHT_MAX_BYTES: usize = 32 * 1024;

fn syntax_set() -> &'static syntect::parsing::SyntaxSet {
    static SS: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    SS.get_or_init(syntect::parsing::SyntaxSet::load_defaults_nonewlines)
}

fn theme() -> &'static syntect::highlighting::Theme {
    static TH: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    TH.get_or_init(|| {
        let mut ts = syntect::highlighting::ThemeSet::load_defaults();
        ts.themes.remove("base16-ocean.dark").unwrap_or_default()
    })
}

/// Highlight code lines; `None` entries mean "plain".
fn highlight(lang: Option<&str>, lines: &[&str]) -> Vec<Vec<(Option<RgbColor>, String)>> {
    let plain = || {
        lines
            .iter()
            .map(|l| vec![(None, (*l).to_string())])
            .collect::<Vec<_>>()
    };
    let Some(lang) = lang else { return plain() };
    if lines.iter().map(|l| l.len()).sum::<usize>() > HIGHLIGHT_MAX_BYTES {
        return plain();
    }
    let ss = syntax_set();
    let Some(syntax) = ss.find_syntax_by_token(lang) else {
        return plain();
    };
    let mut h = syntect::easy::HighlightLines::new(syntax, theme());
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        match h.highlight_line(line, ss) {
            Ok(regions) => out.push(
                regions
                    .into_iter()
                    .map(|(st, text)| {
                        (
                            Some(RgbColor::new(
                                st.foreground.r,
                                st.foreground.g,
                                st.foreground.b,
                            )),
                            text.to_string(),
                        )
                    })
                    .collect(),
            ),
            Err(_) => return plain(),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Inline parser
// ---------------------------------------------------------------------------------------------

const MAX_DEPTH: u8 = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delim {
    Bold,
    Underline,
    Strike,
    Spoiler,
    StarEm,
    UnderEm,
}

impl Delim {
    const fn token(self) -> &'static [u8] {
        match self {
            Self::Bold => b"**",
            Self::Underline => b"__",
            Self::Strike => b"~~",
            Self::Spoiler => b"||",
            Self::StarEm => b"*",
            Self::UnderEm => b"_",
        }
    }
    const fn apply(self, mut s: MdStyle) -> MdStyle {
        match self {
            Self::Bold => s.bold = true,
            Self::Underline => s.underline = true,
            Self::Strike => s.strike = true,
            Self::Spoiler => s.spoiler = true,
            Self::StarEm | Self::UnderEm => s.italic = true,
        }
        s
    }
}

const fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Find the closing delimiter for `d` whose content starts at `start`. Returns its byte index.
fn find_closer(b: &[u8], start: usize, d: Delim) -> Option<usize> {
    let tok = d.token();
    let ch = tok[0];
    let mut j = start;
    while j + tok.len() <= b.len() {
        if b[j] == b'\\' {
            j += 2;
            continue;
        }
        if tok.len() == 1 && j + 1 < b.len() && b[j] == ch && b[j + 1] == ch {
            // Inside single emphasis a doubled delimiter is consumed as a unit.
            j += 2;
            continue;
        }
        if b[j..].starts_with(tok) && j > start {
            let after = j + tok.len();
            let followed_same = after < b.len() && b[after] == ch;
            let ok = match d {
                Delim::Bold | Delim::Underline => !followed_same,
                Delim::Strike | Delim::Spoiler => true,
                Delim::StarEm => !followed_same && !b[j - 1].is_ascii_whitespace(),
                Delim::UnderEm => !followed_same && (after >= b.len() || !is_word(b[after])),
            };
            if ok {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

fn parse_id(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn is_emoji_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'~')
}

/// Days since 1970-01-01 → (year, month, day) (proleptic Gregorian, H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Render a Discord timestamp in UTC. `R` (relative) renders absolute to stay deterministic.
fn format_timestamp(unix: i64, style: u8) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mon = MONTHS[(m as usize).saturating_sub(1) % 12];
    let wd = DAYS[usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0)];
    match style {
        b't' => format!("{hh:02}:{mm:02} UTC"),
        b'T' => format!("{hh:02}:{mm:02}:{ss:02} UTC"),
        b'd' => format!("{y:04}-{m:02}-{d:02}"),
        b'D' => format!("{d} {mon} {y}"),
        b'F' => format!("{wd}, {d} {mon} {y} {hh:02}:{mm:02} UTC"),
        b'R' => format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC"),
        _ => format!("{d} {mon} {y} {hh:02}:{mm:02} UTC"),
    }
}

/// Bare/angle URL scheme check (case-insensitive).
fn url_scheme_len(s: &str) -> Option<usize> {
    let head = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    if head.starts_with("https://") {
        Some(8)
    } else if head.starts_with("http://") {
        Some(7)
    } else {
        None
    }
}

/// Length in bytes of a bare URL at the start of `s` (after trailing-punctuation trimming).
fn bare_url_len(s: &str) -> Option<usize> {
    let scheme = url_scheme_len(s)?;
    let mut end = s
        .find(|c: char| c.is_whitespace() || c == '<' || c == '>')
        .unwrap_or(s.len());
    loop {
        let cur = &s[..end];
        let Some(last) = cur.chars().last() else {
            break;
        };
        if ".,:;!?\"'*_~|".contains(last) {
            end -= last.len_utf8();
        } else if last == ')' && cur.matches('(').count() < cur.matches(')').count() {
            end -= 1;
        } else {
            break;
        }
    }
    (end > scheme).then_some(end)
}

enum Angle {
    Mention(MentionKind, u64),
    Emoji(String),
    Timestamp(i64, u8),
    Command(String),
    Url(String),
}

/// Parse `<…>` constructs at the start of `s`; returns (consumed bytes, construct).
fn parse_angle(s: &str) -> Option<(usize, Angle)> {
    let close = s.find('>')?;
    let inner = &s[1..close];
    if inner.is_empty() || inner.len() > 2100 {
        return None;
    }
    let consumed = close + 1;
    if let Some(r) = inner.strip_prefix("@&") {
        return parse_id(r).map(|id| (consumed, Angle::Mention(MentionKind::Role, id)));
    }
    if let Some(r) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        return parse_id(r).map(|id| (consumed, Angle::Mention(MentionKind::User, id)));
    }
    if let Some(r) = inner.strip_prefix('#') {
        return parse_id(r).map(|id| (consumed, Angle::Mention(MentionKind::Channel, id)));
    }
    if let Some(r) = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':')) {
        let (name, id) = r.split_once(':')?;
        return (is_emoji_name(name) && parse_id(id).is_some())
            .then(|| (consumed, Angle::Emoji(name.to_string())));
    }
    if let Some(r) = inner.strip_prefix("t:") {
        let (num, style) = match r.split_once(':') {
            Some((n, st)) if st.len() == 1 && "tTdDfFR".contains(st) => (n, st.as_bytes()[0]),
            Some(_) => return None,
            None => (r, b'f'),
        };
        let digits = num.strip_prefix('-').unwrap_or(num);
        if digits.is_empty() || digits.len() > 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: i64 = num.parse().ok()?;
        return (v.abs() <= 8_640_000_000_000).then_some((consumed, Angle::Timestamp(v, style)));
    }
    if let Some(r) = inner.strip_prefix('/') {
        let (name, id) = r.rsplit_once(':')?;
        let ok_name = !name.is_empty()
            && name.len() <= 100
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ' ');
        return (ok_name && parse_id(id).is_some())
            .then(|| (consumed, Angle::Command(name.to_string())));
    }
    if url_scheme_len(inner).is_some() && !inner.contains(char::is_whitespace) {
        return Some((consumed, Angle::Url(inner.to_string())));
    }
    None
}

/// Parse `[text](url)` at the start of `s`; returns (consumed, text, url).
fn parse_masked_link(s: &str) -> Option<(usize, &str, &str)> {
    let b = s.as_bytes();
    let mut j = 1;
    while j < b.len() && b[j] != b']' {
        if b[j] == b'\\' {
            j += 1;
        } else if b[j] == b'[' {
            return None;
        }
        j += 1;
    }
    if j >= b.len() || j == 1 || b.get(j + 1) != Some(&b'(') {
        return None;
    }
    let text = &s[1..j];
    let ustart = j + 2;
    let mut depth = 0i32;
    let mut k = ustart;
    while k < b.len() {
        match b[k] {
            b'(' => depth += 1,
            b')' if depth == 0 => break,
            b')' => depth -= 1,
            _ => {}
        }
        k += 1;
    }
    if k >= b.len() {
        return None;
    }
    let mut url = s[ustart..k].trim();
    // Optional title: `(url "title")`.
    if let Some((u, t)) = url.split_once(char::is_whitespace)
        && t.trim_start().starts_with('"')
    {
        url = u;
    }
    let url = url
        .strip_prefix('<')
        .and_then(|u| u.strip_suffix('>'))
        .unwrap_or(url);
    Some((k + 1, text, url))
}

struct Inline<'a> {
    ctx: &'a TextCtx,
    resolve: Resolver<'a>,
    out: Vec<MdSpan>,
}

impl Inline<'_> {
    fn push(&mut self, text: &str, style: MdStyle, url: Option<&str>) {
        if text.is_empty() {
            return;
        }
        let t = normalize(text, self.ctx);
        if t.is_empty() {
            return;
        }
        if let Some(last) = self.out.last_mut()
            && last.style == style
            && last.url.as_deref() == url
        {
            last.text.push_str(&t);
            return;
        }
        self.out.push(MdSpan {
            text: t.into_owned(),
            style,
            url: url.map(str::to_string),
        });
    }

    fn mention_text(&self, kind: MentionKind, id: u64) -> String {
        let sigil = if kind == MentionKind::Channel {
            '#'
        } else {
            '@'
        };
        match (self.resolve)(kind, id) {
            Some(name) if !name.trim().is_empty() => {
                let name = sanitize_single_line(&name);
                if name.starts_with(sigil) {
                    name.into_owned()
                } else {
                    format!("{sigil}{name}")
                }
            }
            _ => format!("{sigil}unknown"),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse(&mut self, s: &str, style: MdStyle, url: Option<&str>, depth: u8) {
        let b = s.as_bytes();
        let mut lit = String::new();
        let mut i = 0usize;
        macro_rules! flush {
            () => {
                if !lit.is_empty() {
                    let t = std::mem::take(&mut lit);
                    self.push(&t, style, url);
                }
            };
        }
        while i < b.len() {
            let c = b[i];
            let prev_word = i > 0 && is_word(b[i - 1]);
            match c {
                b'\\' => {
                    if i + 1 < b.len() && b[i + 1].is_ascii_punctuation() {
                        lit.push(char::from(b[i + 1]));
                        i += 2;
                    } else {
                        lit.push('\\');
                        i += 1;
                    }
                    continue;
                }
                b'`' => {
                    let n = b[i..].iter().take_while(|&&x| x == b'`').count();
                    let from = i + n;
                    let mut k = from;
                    let mut found = None;
                    while k < b.len() {
                        if b[k] == b'`' {
                            let m = b[k..].iter().take_while(|&&x| x == b'`').count();
                            if m == n && k > from {
                                found = Some(k);
                                break;
                            }
                            k += m;
                        } else {
                            k += 1;
                        }
                    }
                    if let Some(k) = found {
                        flush!();
                        let mut code = &s[from..k];
                        if code.len() >= 2
                            && code.starts_with(' ')
                            && code.ends_with(' ')
                            && !code.trim().is_empty()
                        {
                            code = &code[1..code.len() - 1];
                        }
                        self.push(
                            code,
                            MdStyle {
                                code: true,
                                ..style
                            },
                            url,
                        );
                        i = k + n;
                    } else {
                        lit.push_str(&s[i..from]);
                        i = from;
                    }
                    continue;
                }
                b'<' => {
                    if let Some((n, a)) = parse_angle(&s[i..]) {
                        flush!();
                        match a {
                            Angle::Mention(kind, id) => {
                                let t = self.mention_text(kind, id);
                                self.push(
                                    &t,
                                    MdStyle {
                                        mention: true,
                                        ..style
                                    },
                                    url,
                                );
                            }
                            Angle::Emoji(name) => self.push(&format!(":{name}:"), style, url),
                            Angle::Timestamp(v, st) => {
                                self.push(
                                    &format_timestamp(v, st),
                                    MdStyle {
                                        mention: true,
                                        ..style
                                    },
                                    url,
                                );
                            }
                            Angle::Command(name) => self.push(
                                &format!("/{name}"),
                                MdStyle {
                                    mention: true,
                                    ..style
                                },
                                url,
                            ),
                            Angle::Url(u) => match sanitize_osc8_url(&u) {
                                Some(clean) => self.push(
                                    &u,
                                    MdStyle {
                                        link: true,
                                        ..style
                                    },
                                    Some(&clean),
                                ),
                                None => self.push(&u, style, url),
                            },
                        }
                        i += n;
                        continue;
                    }
                }
                b'[' if url.is_none() => {
                    if let Some((n, text, target)) = parse_masked_link(&s[i..])
                        && let Some(clean) = sanitize_osc8_url(target)
                    {
                        flush!();
                        let st = MdStyle {
                            link: true,
                            ..style
                        };
                        if depth < MAX_DEPTH {
                            self.parse(text, st, Some(&clean), depth + 1);
                        } else {
                            self.push(text, st, Some(&clean));
                        }
                        i += n;
                        continue;
                    }
                }
                b'h' | b'H' if url.is_none() && !(i > 0 && b[i - 1].is_ascii_alphanumeric()) => {
                    if let Some(n) = bare_url_len(&s[i..]) {
                        flush!();
                        let raw = &s[i..i + n];
                        match sanitize_osc8_url(raw) {
                            Some(clean) => self.push(
                                raw,
                                MdStyle {
                                    link: true,
                                    ..style
                                },
                                Some(&clean),
                            ),
                            None => self.push(raw, style, url),
                        }
                        i += n;
                        continue;
                    }
                }
                b'@' if !prev_word => {
                    let rest = &s[i..];
                    let hit = ["@everyone", "@here"].into_iter().find(|m| {
                        rest.starts_with(m)
                            && !rest.as_bytes().get(m.len()).is_some_and(|&x| is_word(x))
                    });
                    if let Some(m) = hit {
                        flush!();
                        self.push(
                            m,
                            MdStyle {
                                mention: true,
                                ..style
                            },
                            url,
                        );
                        i += m.len();
                        continue;
                    }
                }
                b'*' | b'_' | b'~' | b'|' if depth < MAX_DEPTH => {
                    let doubled = i + 1 < b.len() && b[i + 1] == c;
                    let cands: &[Delim] = match (c, doubled) {
                        (b'*', true) => &[Delim::Bold, Delim::StarEm],
                        (b'*', false) => &[Delim::StarEm],
                        (b'_', true) => &[Delim::Underline, Delim::UnderEm],
                        (b'_', false) => &[Delim::UnderEm],
                        (b'~', true) => &[Delim::Strike],
                        (b'|', true) => &[Delim::Spoiler],
                        _ => &[],
                    };
                    let mut matched = false;
                    for &d in cands {
                        let tl = d.token().len();
                        let start = i + tl;
                        if start >= b.len() {
                            continue;
                        }
                        match d {
                            Delim::StarEm if b[start].is_ascii_whitespace() => continue,
                            Delim::UnderEm | Delim::Underline if prev_word => continue,
                            _ => {}
                        }
                        if let Some(j) = find_closer(b, start, d) {
                            flush!();
                            self.parse(&s[start..j], d.apply(style), url, depth + 1);
                            i = j + tl;
                            matched = true;
                            break;
                        }
                    }
                    if matched {
                        continue;
                    }
                    // Unmatched run: emit literally (whole run, so `**` never re-opens as `*`).
                    let n = b[i..].iter().take_while(|&&x| x == c).count();
                    lit.push_str(&s[i..i + n]);
                    i += n;
                    continue;
                }
                _ => {}
            }
            // Literal char (full UTF-8 scalar).
            let ch = s[i..].chars().next().unwrap_or(' ');
            lit.push(ch);
            i += ch.len_utf8();
        }
        flush!();
    }
}

fn inline_spans(s: &str, base: MdStyle, ctx: &TextCtx, resolve: Resolver<'_>) -> Vec<MdSpan> {
    let mut p = Inline {
        ctx,
        resolve,
        out: Vec::new(),
    };
    p.parse(s, base, None, 0);
    p.out
}

// ---------------------------------------------------------------------------------------------
// Block parser
// ---------------------------------------------------------------------------------------------

fn parse_fence_lang(after: &str) -> (Option<String>, Option<&str>) {
    let t = after.trim();
    if t.is_empty() {
        return (None, None);
    }
    if t.len() <= 32
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-#_.".contains(&b))
    {
        (Some(t.to_string()), None)
    } else {
        (None, Some(after))
    }
}

fn list_marker(line: &str) -> Option<(u8, &str, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let depth = u8::try_from((indent / 2).min(7)).unwrap_or(7);
    if let Some(r) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
        return Some((depth, "• ", r));
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=9).contains(&digits) && rest[digits..].starts_with(". ") {
        return Some((depth, &rest[..digits + 2], &rest[digits + 2..]));
    }
    None
}

/// Parse Discord Markdown into logical lines. `src` must already be sanitized.
///
/// Mentions are resolved through `resolve` (names may be given with or without the `@`/`#`
/// sigil; unresolved → `@unknown` / `#unknown`). List items include their indentation and
/// bullet in the spans; quote bars are left to the caller (`LineKind::Quote`).
#[must_use]
pub fn render_markdown(
    src: &str,
    ctx: &TextCtx,
    resolve: &dyn Fn(MentionKind, u64) -> Option<String>,
) -> Vec<MdLine> {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut multi_quote = false;
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let (quote, body) = if multi_quote {
            (true, raw)
        } else if let Some(r) = raw.strip_prefix(">>> ") {
            multi_quote = true;
            (true, r)
        } else if raw == ">>>" {
            multi_quote = true;
            (true, "")
        } else if let Some(r) = raw.strip_prefix("> ") {
            (true, r)
        } else {
            (false, raw)
        };

        // Fenced code block (must be closed later, else it is literal text).
        let trimmed = body.trim_start();
        if (!quote || multi_quote)
            && trimmed.starts_with("```")
            && !trimmed[3..].contains("```")
            && let Some(close) = (i + 1..lines.len()).find(|&k| lines[k].contains("```"))
        {
            {
                let (lang, first) = parse_fence_lang(&trimmed[3..]);
                let mut code: Vec<&str> = Vec::new();
                if let Some(f) = first {
                    code.push(f);
                }
                code.extend(&lines[i + 1..close]);
                let tail = lines[close].split("```").next().unwrap_or("");
                if !tail.is_empty() {
                    code.push(tail);
                }
                for regions in highlight(lang.as_deref(), &code) {
                    let mut spans: Vec<MdSpan> = Vec::new();
                    for (fg, text) in regions {
                        let t = normalize(&text, ctx);
                        if t.is_empty() {
                            continue;
                        }
                        let style = MdStyle {
                            code: true,
                            fg,
                            ..MdStyle::default()
                        };
                        match spans.last_mut() {
                            Some(l) if l.style == style => l.text.push_str(&t),
                            _ => spans.push(MdSpan {
                                text: t.into_owned(),
                                style,
                                url: None,
                            }),
                        }
                    }
                    out.push(MdLine {
                        kind: LineKind::Code { lang: lang.clone() },
                        spans,
                    });
                }
                // Text after the closing fence on the same line becomes a normal line.
                if let Some((_, after)) = lines[close].split_once("```")
                    && !after.trim().is_empty()
                {
                    out.push(MdLine {
                        kind: LineKind::Normal,
                        spans: inline_spans(after, MdStyle::default(), ctx, resolve),
                    });
                }
                i = close + 1;
                continue;
            }
        }

        let line = if quote {
            let (base, text) = match body.strip_prefix("-# ") {
                Some(r) => (
                    MdStyle {
                        subtext: true,
                        ..MdStyle::default()
                    },
                    r,
                ),
                None => (MdStyle::default(), body),
            };
            MdLine {
                kind: LineKind::Quote,
                spans: inline_spans(text, base, ctx, resolve),
            }
        } else if let Some((lvl, r)) = [("### ", 3u8), ("## ", 2), ("# ", 1)]
            .into_iter()
            .find_map(|(p, l)| body.strip_prefix(p).map(|r| (l, r)))
        {
            MdLine {
                kind: LineKind::Heading(lvl),
                spans: inline_spans(r, MdStyle::default(), ctx, resolve),
            }
        } else if let Some(r) = body.strip_prefix("-# ") {
            MdLine {
                kind: LineKind::Normal,
                spans: inline_spans(
                    r,
                    MdStyle {
                        subtext: true,
                        ..MdStyle::default()
                    },
                    ctx,
                    resolve,
                ),
            }
        } else if let Some((depth, bullet, r)) = list_marker(body) {
            let mut spans = Vec::new();
            let prefix = format!("{}{bullet}", "  ".repeat(usize::from(depth)));
            spans.push(MdSpan {
                text: normalize(&prefix, ctx).into_owned(),
                style: MdStyle::default(),
                url: None,
            });
            spans.extend(inline_spans(r, MdStyle::default(), ctx, resolve));
            MdLine {
                kind: LineKind::ListItem(depth),
                spans,
            }
        } else {
            MdLine {
                kind: LineKind::Normal,
                spans: inline_spans(body, MdStyle::default(), ctx, resolve),
            }
        };
        out.push(line);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Span-aware wrapping
// ---------------------------------------------------------------------------------------------

/// Wrap logical lines to `width` cells, never splitting a grapheme (I4).
///
/// Same rules as [`crate::text::wrap`] (word-wrap at spaces, spaces at a break consumed,
/// overlong words hard-broken); `Code` lines hard-wrap keeping every grapheme. Styles and URLs
/// are preserved across breaks. `start_grapheme` is the offset in the rendered text of the whole
/// message (each logical line's graphemes, plus one per joining `\n`) — width-independent, so
/// it is the scroll-anchor key (§5).
#[must_use]
pub fn wrap_lines(lines: &[MdLine], width: usize) -> Vec<StyledLine> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut base = 0usize;
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.text.as_str()).collect();
        // Map each grapheme to its span by byte offset.
        let mut bounds = Vec::with_capacity(line.spans.len());
        let mut acc = 0usize;
        for s in &line.spans {
            acc += s.text.len();
            bounds.push(acc);
        }
        let mut gs: Vec<&str> = Vec::new();
        let mut owner: Vec<usize> = Vec::new();
        let mut span_idx = 0usize;
        for (off, g) in text.grapheme_indices(true) {
            while span_idx + 1 < bounds.len() && off >= bounds[span_idx] {
                span_idx += 1;
            }
            gs.push(g);
            owner.push(span_idx);
        }
        let widths: Vec<usize> = gs.iter().map(|g| grapheme_width(g)).collect();
        let hard = matches!(line.kind, LineKind::Code { .. });
        for r in wrap_ranges(&gs, &widths, width, hard) {
            let mut spans: Vec<MdSpan> = Vec::new();
            let mut w = 0usize;
            for k in r.clone() {
                let src = &line.spans[owner[k]];
                let (g, gw) = if widths[k] > width {
                    (" ", 1)
                } else {
                    (gs[k], widths[k])
                };
                w += gw;
                match spans.last_mut() {
                    Some(l) if l.style == src.style && l.url == src.url => l.text.push_str(g),
                    _ => spans.push(MdSpan {
                        text: g.to_string(),
                        style: src.style,
                        url: src.url.clone(),
                    }),
                }
            }
            out.push(StyledLine {
                kind: line.kind.clone(),
                spans,
                start_grapheme: u32::try_from(base + r.start).unwrap_or(u32::MAX),
                width: u16::try_from(w).unwrap_or(u16::MAX),
            });
        }
        base += gs.len() + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbs_core::ResolvedCharset;

    fn res(kind: MentionKind, id: u64) -> Option<String> {
        match (kind, id) {
            (MentionKind::User, 1) => Some("ali".into()),
            (MentionKind::Role, 2) => Some("mods".into()),
            (MentionKind::Channel, 3) => Some("general".into()),
            (MentionKind::User, 4) => Some("evil\u{1b}[2J\nname".into()),
            _ => None,
        }
    }

    fn md(s: &str) -> Vec<MdLine> {
        render_markdown(s, &TextCtx::default(), &res)
    }

    fn text(l: &MdLine) -> String {
        l.spans.iter().map(|s| s.text.as_str()).collect()
    }

    fn find<'a>(l: &'a MdLine, t: &str) -> &'a MdSpan {
        l.spans
            .iter()
            .find(|s| s.text == t)
            .unwrap_or_else(|| panic!("no span {t:?} in {l:?}"))
    }

    #[test]
    fn emphasis() {
        let l = &md("a **b** *i* _j_ __u__ ~~s~~ ||sp|| `c`")[0];
        assert_eq!(text(l), "a b i j u s sp c");
        assert!(find(l, "b").style.bold);
        assert!(find(l, "i").style.italic);
        assert!(find(l, "j").style.italic);
        let u = find(l, "u").style;
        assert!(u.underline && !u.bold);
        assert!(find(l, "s").style.strike);
        assert!(find(l, "sp").style.spoiler);
        assert!(find(l, "c").style.code);
    }

    #[test]
    fn nesting_and_edge_cases() {
        let l = &md("***bi***")[0];
        let s = find(l, "bi").style;
        assert!(s.bold && s.italic);
        let l = &md("**bold _it_**")[0];
        assert!(find(l, "it").style.bold && find(l, "it").style.italic);
        assert_eq!(text(&md("snake_case_name")[0]), "snake_case_name");
        assert_eq!(text(&md("2 * 3 * 4")[0]), "2 * 3 * 4");
        assert_eq!(text(&md("**unclosed")[0]), "**unclosed");
        assert_eq!(text(&md(r"\*not\* \\ x")[0]), r"*not* \ x");
        assert_eq!(text(&md("`**raw**`")[0]), "**raw**");
        assert_eq!(text(&md("``a`b``")[0]), "a`b");
        let l = &md("*a **b** c*")[0];
        assert!(find(l, "b").style.bold && find(l, "b").style.italic);
        // Pathological input stays bounded and does not panic.
        let evil = "*_~|".repeat(500);
        let _ = md(&evil);
        let deep = "**".repeat(200) + "x" + &"**".repeat(200);
        let _ = md(&deep);
    }

    #[test]
    fn blocks() {
        let ls = md("# H1\n## H2\n### H3\n#### no\n-# small\n> q\n- a\n  * b\n1. c\nplain");
        assert_eq!(ls[0].kind, LineKind::Heading(1));
        assert_eq!(text(&ls[0]), "H1");
        assert_eq!(ls[2].kind, LineKind::Heading(3));
        assert_eq!(ls[3].kind, LineKind::Normal);
        assert!(ls[4].spans[0].style.subtext);
        assert_eq!(ls[5].kind, LineKind::Quote);
        assert_eq!(text(&ls[5]), "q");
        assert_eq!(ls[6].kind, LineKind::ListItem(0));
        assert_eq!(text(&ls[6]), "• a");
        assert_eq!(ls[7].kind, LineKind::ListItem(1));
        assert_eq!(text(&ls[7]), "  • b");
        assert_eq!(text(&ls[8]), "1. c");
        assert_eq!(ls[9].kind, LineKind::Normal);
        let ls = md("x\n>>> a\nb\nc");
        assert_eq!(ls[0].kind, LineKind::Normal);
        assert!(ls[1..].iter().all(|l| l.kind == LineKind::Quote));
        assert_eq!(text(&ls[1]), "a");
    }

    #[test]
    fn code_blocks() {
        let ls = md("before\n```rust\nfn main() {}\n\nlet x = 1;\n```\nafter");
        assert_eq!(ls.len(), 5);
        assert_eq!(
            ls[1].kind,
            LineKind::Code {
                lang: Some("rust".into())
            }
        );
        assert_eq!(text(&ls[1]), "fn main() {}");
        assert!(
            ls[1]
                .spans
                .iter()
                .all(|s| s.style.code && s.style.fg.is_some())
        );
        assert!(
            ls[1].spans.len() > 1,
            "highlighted into several spans: {:?}",
            ls[1]
        );
        assert!(ls[2].spans.is_empty());
        assert_eq!(text(&ls[4]), "after");
        // Unknown language: plain code.
        let ls = md("```nosuchlang\n**x**\n```");
        assert_eq!(text(&ls[0]), "**x**");
        assert!(ls[0].spans[0].style.code && ls[0].spans[0].style.fg.is_none());
        // Unclosed fence is literal.
        let ls = md("```\nnot code");
        assert_eq!(ls[0].kind, LineKind::Normal);
        // Single-line fence is inline code.
        let ls = md("```x = 1```");
        assert!(ls[0].spans[0].style.code);
        assert_eq!(text(&ls[0]), "x = 1");
    }

    #[test]
    fn links_and_mentions() {
        let l = &md("see [docs](https://ex.com/a b) and https://x.y/p_(q). <https://no.embed>")[0];
        let d = find(l, "docs");
        assert!(d.style.link);
        assert_eq!(d.url.as_deref(), Some("https://ex.com/a%20b"));
        let u = find(l, "https://x.y/p_(q)");
        assert_eq!(u.url.as_deref(), Some("https://x.y/p_(q)"));
        assert!(text(l).ends_with("). https://no.embed"));
        let l = &md("[bad](javascript:alert(1))")[0];
        assert!(l.spans.iter().all(|s| s.url.is_none()));
        let l = &md("<@1> <@!1> <@&2> <#3> <@99> <#98> @everyone a@here")[0];
        assert_eq!(
            text(l),
            "@ali @ali @mods #general @unknown #unknown @everyone a@here"
        );
        assert!(find(l, "@mods").style.mention);
        assert!(find(l, "@everyone").style.mention);
        let l = &md("<@4>")[0];
        assert!(!text(l).contains('\u{1b}') && !text(l).contains('\n'));
        let l = &md("hi <:pepe:123> <a:party:456> </ban user:789>")[0];
        assert_eq!(text(l), "hi :pepe: :party: /ban user");
    }

    #[test]
    fn timestamps() {
        let l = &md("<t:1618953630:F> <t:1618953630:d> <t:1618953630> <t:0:t> <t:-86400:D>")[0];
        assert_eq!(
            text(l),
            "Tue, 20 Apr 2021 21:20 UTC 2021-04-20 20 Apr 2021 21:20 UTC 00:00 UTC 31 Dec 1969"
        );
        assert_eq!(
            text(&md("<t:99999999999999999999>")[0]),
            "<t:99999999999999999999>"
        );
    }

    #[test]
    fn ascii_tier_normalizes_spans() {
        let c = TextCtx::new(ResolvedCharset::Ascii7Bit);
        let ls = render_markdown("**Café** 😀 <:x:1> `a\\b`\n- ä\n```\n中\n```", &c, &res);
        for l in &ls {
            for s in &l.spans {
                assert!(
                    s.text
                        .bytes()
                        .all(|b| (0x20..=0x7D).contains(&b) && b != 0x5C),
                    "{s:?}"
                );
            }
        }
        assert_eq!(text(&ls[0]), "Cafe :grinning: :x: a/b");
        assert_eq!(text(&ls[1]), "* a");
    }

    #[test]
    fn wrap_preserves_styles_and_anchor_keys() {
        let ls = md(
            "hello **bold world here** and _more text_ 中文中文 😀😀\n```\n    indented code line\n```\nend",
        );
        let total: usize = ls.iter().map(|l| text(l).graphemes(true).count() + 1).sum();
        for w in 1..=80 {
            let wrapped = wrap_lines(&ls, w);
            let mut prev: Option<u32> = None;
            for sl in &wrapped {
                assert!(usize::from(sl.width) <= w, "width {w}: {sl:?}");
                let lw: usize = sl
                    .spans
                    .iter()
                    .map(|s| crate::text::str_width(&s.text))
                    .sum();
                assert_eq!(lw, usize::from(sl.width));
                if let Some(p) = prev {
                    assert!(sl.start_grapheme > p, "monotone at {w}");
                }
                prev = Some(sl.start_grapheme);
                assert!((sl.start_grapheme as usize) < total);
            }
            // Bold survives breaks.
            let bold: String = wrapped
                .iter()
                .flat_map(|l| &l.spans)
                .filter(|s| s.style.bold)
                .map(|s| s.text.as_str())
                .collect();
            if w >= 2 {
                assert_eq!(bold.replace(' ', ""), "boldworldhere");
            }
            // Code lines keep all graphemes (incl. indentation).
            let code: String = wrapped
                .iter()
                .filter(|l| matches!(l.kind, LineKind::Code { .. }))
                .flat_map(|l| &l.spans)
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(code, "    indented code line");
            // Line starts at logical boundaries are identical across widths.
            let starts: Vec<u32> = ls
                .iter()
                .scan(0u32, |acc, l| {
                    let s = *acc;
                    *acc += text(l).graphemes(true).count() as u32 + 1;
                    Some(s)
                })
                .collect();
            for s in starts {
                assert!(
                    wrapped.iter().any(|l| l.start_grapheme == s),
                    "logical start {s} at width {w}"
                );
            }
        }
    }
}
