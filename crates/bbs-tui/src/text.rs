//! Display-width, grapheme, emoji-policy, charset normalization and wrapping utilities (§1.3, §1.5, §4.5).
//!
//! Everything here measures in terminal cells over extended grapheme clusters
//! (`unicode-segmentation` + `unicode-width`). The rules:
//!
//! * control characters (incl. `\n`) are 0 cells, zero-width clusters are 0 cells,
//! * a cluster with emoji presentation (VS16, keycap, regional-indicator pair, wide base) is 2 cells,
//! * nothing is ever wider than 2 cells.
//!
//! [`normalize`] applies the emoji policy (§1.5) and the charset tier (§1.3) *before* measuring,
//! so that what we measure is what the terminal will actually draw.

use std::borrow::Cow;
use std::ops::Range;

use bbs_core::ResolvedCharset;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Emoji width policy (§1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmojiPolicy {
    /// Strip VS16 / skin tones / ZWJ tails so widths agree across terminals.
    #[default]
    Strip,
    /// Terminal supports DEC mode 2027 grapheme clustering.
    Mode2027,
    /// Replace emoji with `:shortcode:` text.
    Shortcode,
}

/// Text rendering context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextCtx {
    /// Resolved charset tier.
    pub charset: ResolvedCharset,
    /// Emoji policy.
    pub emoji: EmojiPolicy,
    /// HD44780 A02 ROM (unlocks `\\` and `~` in 7-bit mode).
    pub lcd_a02: bool,
}

impl TextCtx {
    /// New context with default emoji policy.
    #[must_use]
    pub const fn new(charset: ResolvedCharset) -> Self {
        Self {
            charset,
            emoji: EmojiPolicy::Strip,
            lcd_a02: false,
        }
    }
}

impl Default for TextCtx {
    fn default() -> Self {
        Self::new(ResolvedCharset::Utf8)
    }
}

// ---------------------------------------------------------------------------------------------
// Character classes
// ---------------------------------------------------------------------------------------------

const VS16: char = '\u{FE0F}';
const ZWJ: char = '\u{200D}';
const KEYCAP: char = '\u{20E3}';

const fn is_fitzpatrick(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

const fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

const fn is_emoji_tag(c: char) -> bool {
    matches!(c, '\u{E0020}'..='\u{E007F}')
}

/// Code points that can be emoji bases (coarse superset; combined with width/VS16 checks).
const fn is_emoji_range(c: char) -> bool {
    matches!(
        c,
        '\u{1F000}'..='\u{1FAFF}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{2300}'..='\u{23FF}'
            | '\u{2B00}'..='\u{2BFF}'
            | '\u{2190}'..='\u{21FF}'
            | '\u{3030}'
            | '\u{303D}'
            | '\u{3297}'
            | '\u{3299}'
            | '\u{00A9}'
            | '\u{00AE}'
            | '\u{2122}'
            | '\u{2139}'
            | '\u{24C2}'
            | '\u{25AA}'..='\u{25FE}'
            | '\u{2934}'
            | '\u{2935}'
    )
}

/// Does this grapheme cluster render as an emoji (as opposed to a text glyph)?
///
/// A text-default base without VS16 (`❤`, `✓`) is *not* an emoji: it is drawn as a 1-cell glyph.
fn is_emoji_cluster(g: &str) -> bool {
    let Some(first) = g.chars().next() else {
        return false;
    };
    if g.contains(KEYCAP) && matches!(first, '0'..='9' | '#' | '*') {
        return true;
    }
    if is_regional_indicator(first) || is_fitzpatrick(first) {
        return true;
    }
    if !is_emoji_range(first) {
        return false;
    }
    g.contains(VS16) || g.contains(ZWJ) || UnicodeWidthChar::width(first) == Some(2)
}

// ---------------------------------------------------------------------------------------------
// Width
// ---------------------------------------------------------------------------------------------

/// Display width of one grapheme cluster (0..=2).
///
/// Controls and zero-width clusters are 0; emoji-presentation clusters are 2.
#[must_use]
pub fn grapheme_width(g: &str) -> usize {
    let mut chars = g.chars();
    let Some(first) = chars.next() else { return 0 };
    if first.is_control() {
        return 0;
    }
    if is_regional_indicator(first) {
        // A lone RI is drawn narrow by most terminals; a flag pair is wide everywhere.
        return if chars.next().is_some_and(is_regional_indicator) {
            2
        } else {
            1
        };
    }
    let base = UnicodeWidthChar::width(first).unwrap_or(0);
    if base == 0 {
        // Lone combining mark / ZWSP / format char at the start of a cluster.
        return UnicodeWidthStr::width(g).min(2);
    }
    if base == 1
        && (g.contains(VS16) || g.contains(KEYCAP))
        && (is_emoji_range(first) || g.contains(KEYCAP))
    {
        return 2;
    }
    if base == 2 {
        return 2;
    }
    UnicodeWidthStr::width(g).clamp(1, 2)
}

/// Display width in cells.
#[must_use]
pub fn str_width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

// ---------------------------------------------------------------------------------------------
// Normalization: emoji policy + charset tier
// ---------------------------------------------------------------------------------------------

/// Small built-in shortcode table, keyed by the base code point of an emoji cluster.
const SHORTCODES: &[(char, &str)] = &[
    ('😀', "grinning"),
    ('😃', "smiley"),
    ('😄', "smile"),
    ('😁', "grin"),
    ('😆', "laughing"),
    ('😅', "sweat_smile"),
    ('😂', "joy"),
    ('🤣', "rofl"),
    ('😊', "blush"),
    ('😇', "innocent"),
    ('🙂', "slight_smile"),
    ('🙃', "upside_down"),
    ('😉', "wink"),
    ('😍', "heart_eyes"),
    ('🥰', "smiling_face_with_hearts"),
    ('😘', "kissing_heart"),
    ('😋', "yum"),
    ('😛', "stuck_out_tongue"),
    ('😜', "stuck_out_tongue_winking_eye"),
    ('🤪', "zany_face"),
    ('🤔', "thinking"),
    ('🤨', "raised_eyebrow"),
    ('😐', "neutral_face"),
    ('😑', "expressionless"),
    ('😶', "no_mouth"),
    ('😏', "smirk"),
    ('😒', "unamused"),
    ('🙄', "rolling_eyes"),
    ('😬', "grimacing"),
    ('😌', "relieved"),
    ('😔', "pensive"),
    ('😴', "sleeping"),
    ('😷', "mask"),
    ('🤒', "thermometer_face"),
    ('🤢', "nauseated_face"),
    ('🤮', "vomiting_face"),
    ('🥵', "hot_face"),
    ('🥶', "cold_face"),
    ('😵', "dizzy_face"),
    ('🤯', "exploding_head"),
    ('🥳', "partying_face"),
    ('😎', "sunglasses"),
    ('🤓', "nerd"),
    ('😕', "confused"),
    ('😟', "worried"),
    ('🙁', "slight_frown"),
    ('😮', "open_mouth"),
    ('😲', "astonished"),
    ('😳', "flushed"),
    ('🥺', "pleading_face"),
    ('🥲', "smiling_face_with_tear"),
    ('😢', "cry"),
    ('😭', "sob"),
    ('😱', "scream"),
    ('😤', "triumph"),
    ('😡', "rage"),
    ('😠', "angry"),
    ('🤬', "cursing_face"),
    ('😈', "smiling_imp"),
    ('💀', "skull"),
    ('💩', "poop"),
    ('🤡', "clown"),
    ('👻', "ghost"),
    ('👽', "alien"),
    ('🤖', "robot"),
    ('🫡', "saluting_face"),
    ('🫠', "melting_face"),
    ('❤', "heart"),
    ('🧡', "orange_heart"),
    ('💛', "yellow_heart"),
    ('💚', "green_heart"),
    ('💙', "blue_heart"),
    ('💜', "purple_heart"),
    ('🖤', "black_heart"),
    ('💔', "broken_heart"),
    ('💯', "100"),
    ('🔥', "fire"),
    ('✨', "sparkles"),
    ('⭐', "star"),
    ('🌟', "star2"),
    ('🎉', "tada"),
    ('🎊', "confetti_ball"),
    ('👍', "thumbsup"),
    ('👎', "thumbsdown"),
    ('👌', "ok_hand"),
    ('✌', "v"),
    ('🤞', "fingers_crossed"),
    ('👏', "clap"),
    ('🙌', "raised_hands"),
    ('🙏', "pray"),
    ('💪', "muscle"),
    ('👋', "wave"),
    ('🤝', "handshake"),
    ('🤷', "shrug"),
    ('🤦', "facepalm"),
    ('👀', "eyes"),
    ('🧠', "brain"),
    ('👑', "crown"),
    ('💎', "gem"),
    ('✅', "white_check_mark"),
    ('❌', "x"),
    ('⚠', "warning"),
    ('❓', "question"),
    ('❗', "exclamation"),
    ('🚀', "rocket"),
    ('💡', "bulb"),
    ('🎮', "video_game"),
    ('💻', "computer"),
    ('📌', "pushpin"),
    ('🔒', "lock"),
    ('🔗', "link"),
    ('☕', "coffee"),
    ('🍕', "pizza"),
    ('🍺', "beer"),
    ('🐱', "cat"),
    ('🐶', "dog"),
    ('🐛', "bug"),
    ('🌈', "rainbow"),
    ('☀', "sunny"),
    ('🌙', "crescent_moon"),
    ('⚡', "zap"),
    ('🎵', "musical_note"),
    ('🎶', "notes"),
    ('🔊', "loud_sound"),
    ('🔇', "mute"),
    ('🎤', "microphone"),
    ('🎧', "headphones"),
    ('📞', "telephone_receiver"),
    ('💬', "speech_balloon"),
    ('🟢', "green_circle"),
    ('🔴', "red_circle"),
    ('🟡', "yellow_circle"),
    ('👤', "bust_in_silhouette"),
    ('👥', "busts_in_silhouette"),
    ('🏳', "white_flag"),
    ('🏴', "black_flag"),
];

fn shortcode_name(c: char) -> Option<&'static str> {
    SHORTCODES.iter().find(|(k, _)| *k == c).map(|(_, v)| *v)
}

/// Replacement text for an emoji cluster under `Shortcode` / non-UTF-8 tiers.
fn emoji_shortcode(g: &str, out: &mut String, fallback: &str) {
    let mut chars = g.chars();
    let Some(first) = chars.next() else { return };
    if g.contains(KEYCAP) {
        out.push('[');
        out.push(first);
        out.push(']');
        return;
    }
    if is_regional_indicator(first)
        && let Some(second) = chars.next().filter(|c| is_regional_indicator(*c))
    {
        out.push_str(":flag_");
        for c in [first, second] {
            let off = u32::from(c) - 0x1F1E6;
            out.push(char::from(b'a' + u8::try_from(off).unwrap_or(0)));
        }
        out.push(':');
        return;
    }
    if let Some(name) = shortcode_name(first) {
        out.push(':');
        out.push_str(name);
        out.push(':');
    } else {
        out.push_str(fallback);
    }
}

/// Strip-policy rewrite of an emoji cluster (§1.5).
fn emoji_strip(g: &str, out: &mut String) {
    let Some(first) = g.chars().next() else {
        return;
    };
    if g.contains(KEYCAP) {
        out.push('[');
        out.push(first);
        out.push(']');
        return;
    }
    for c in g.chars() {
        if c == ZWJ {
            break; // collapse ZWJ sequences to the first base emoji
        }
        if c == VS16 || is_fitzpatrick(c) || is_emoji_tag(c) {
            continue;
        }
        out.push(c);
    }
}

/// VtSafe mapping of glyphs absent from Linux PSF console fonts.
fn vtsafe_char(c: char) -> Option<&'static str> {
    Some(match c {
        '⁰' => "[0]",
        '⁴' => "[4]",
        '⁵' => "[5]",
        '⁶' => "[6]",
        '⁷' => "[7]",
        '⁸' => "[8]",
        '⁹' => "[9]",
        '◉' | '●' | '⦿' => "*",
        '▶' | '▸' | '⏵' => ">",
        '◀' | '◂' | '⏴' => "<",
        '✓' | '✔' => "OK",
        '✗' | '✘' => "x",
        '⟪' => "<<",
        '⟫' => ">>",
        '↩' | '↵' => "<-",
        '…' => "...",
        '╭' => "┌",
        '╮' => "┐",
        '╯' => "┘",
        '╰' => "└",
        '\u{2800}' => " ",
        '\u{2801}'..='\u{28FF}' => "▒",
        '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | VS16 => "",
        _ => return None,
    })
}

/// Latin Extended-A U+0100..=U+017F base letters.
const LATIN_EXT_A: &[u8; 128] = b"AaAaAaCcCcCcCcDdDdEeEeEeEeEeGgGgGgGgHhHhIiIiIiIiIiIiJjKkkLlLlLlLlLlNnNnNnnNnOoOoOoOoRrRrRrSsSsSsSsTtTtTtUuUuUuUuUuUuWwYyYZzZzZzs";

/// Latin-1 Supplement U+00A0..=U+00FF transliterations.
const LATIN_1: [&str; 96] = [
    " ", "!", "c", "L", "*", "Y", "|", "S", "\"", "(c)", "a", "<<", "!", "", "(R)",
    "-", // A0..AF
    "o", "+-", "2", "3", "'", "u", "P", ".", ",", "1", "o", ">>", "1/4", "1/2", "3/4",
    "?", // B0..BF
    "A", "A", "A", "A", "A", "A", "AE", "C", "E", "E", "E", "E", "I", "I", "I", "I", // C0..CF
    "D", "N", "O", "O", "O", "O", "O", "x", "O", "U", "U", "U", "U", "Y", "Th",
    "ss", // D0..DF
    "a", "a", "a", "a", "a", "a", "ae", "c", "e", "e", "e", "e", "i", "i", "i", "i", // E0..EF
    "d", "n", "o", "o", "o", "o", "o", "/", "o", "u", "u", "u", "u", "y", "th", "y", // F0..FF
];

fn box_drawing(c: char) -> &'static str {
    match c {
        '\u{2500}' | '\u{2501}' | '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}'
        | '\u{254C}' | '\u{254D}' | '\u{2574}' | '\u{2576}' | '\u{2578}' | '\u{257A}'
        | '\u{257C}' | '\u{257E}' => "-",
        '\u{2550}' => "=",
        '\u{2502}' | '\u{2503}' | '\u{2506}' | '\u{2507}' | '\u{250A}' | '\u{250B}'
        | '\u{254E}' | '\u{254F}' | '\u{2551}' | '\u{2575}' | '\u{2577}' | '\u{2579}'
        | '\u{257B}' | '\u{257D}' | '\u{257F}' => "|",
        '\u{2571}' => "/",
        '\u{2572}' => "/",
        '\u{2573}' => "X",
        _ => "+",
    }
}

/// Transliterate one char into 7-bit ASCII (before the HD44780 A00 byte filter).
fn ascii_translit(c: char, out: &mut String) {
    let cp = u32::from(c);
    let s: &str = match c {
        ' '..='~' | '\n' => {
            out.push(c);
            return;
        }
        '\t' => " ",
        '\u{00A0}'..='\u{00FF}' => LATIN_1[(cp - 0xA0) as usize],
        '\u{0132}' => "IJ",
        '\u{0133}' => "ij",
        '\u{0152}' => "OE",
        '\u{0153}' => "oe",
        '\u{0149}' => "'n",
        '\u{0100}'..='\u{017F}' => {
            out.push(char::from(LATIN_EXT_A[(cp - 0x100) as usize]));
            return;
        }
        '\u{0300}'..='\u{036F}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2060}'
        | '\u{FEFF}'
        | VS16
        | '\u{FE0E}' => "",
        '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => " ",
        '‘' | '’' | '‚' | '‛' | '′' => "'",
        '“' | '”' | '„' | '‟' | '″' => "\"",
        '‹' => "<",
        '›' => ">",
        '‐' | '‑' | '‒' | '–' | '—' | '―' | '−' | '⁃' => "-",
        '…' => "...",
        '•' | '◉' | '●' | '⦿' | '∙' | '⋅' => "*",
        '‣' | '▶' | '▸' | '►' | '⏵' => ">",
        '◀' | '◂' | '◄' | '⏴' => "<",
        '▲' | '△' => "^",
        '▼' | '▽' => "v",
        '○' | '◯' => "o",
        '■' | '□' | '▪' | '▫' => "#",
        '✓' | '✔' => "OK",
        '✗' | '✘' => "x",
        '♪' | '♫' => "#",
        '❤' | '♥' => "<3",
        '€' => "EUR",
        '™' => "TM",
        '←' => "<-",
        '→' => "->",
        '↑' => "^",
        '↓' => "v",
        '↔' => "<->",
        '↕' => "|",
        '⇐' => "<=",
        '⇒' => "=>",
        '⇔' => "<=>",
        '↩' | '↵' => "<-",
        '⟪' => "<<",
        '⟫' => ">>",
        '⁰' => "[0]",
        '⁴' => "[4]",
        '⁵' => "[5]",
        '⁶' => "[6]",
        '⁷' => "[7]",
        '⁸' => "[8]",
        '⁹' => "[9]",
        '≤' => "<=",
        '≥' => ">=",
        '≠' => "!=",
        '≈' => "=",
        '\u{2500}'..='\u{257F}' => box_drawing(c),
        '█' | '▓' | '▉' | '▊' | '▀' | '▄' => "#",
        '▒' => ":",
        '░' => ".",
        '▌' | '▐' | '▍' | '▎' | '▏' => "|",
        '\u{2581}'..='\u{2587}' => "_",
        '\u{2580}'..='\u{259F}' => "#",
        '\u{2800}' => " ",
        '\u{2801}'..='\u{28FF}' => "#",
        '\u{FF01}'..='\u{FF5E}' => {
            // Fullwidth ASCII → ASCII.
            if let Some(a) = char::from_u32(cp - 0xFEE0) {
                out.push(a);
            }
            return;
        }
        c if c.is_control() => "",
        c if UnicodeWidthChar::width(c) == Some(0) => "",
        _ => "?",
    };
    out.push_str(s);
}

/// Final HD44780 A00 byte filter: only `0x20..=0x7D` minus `0x5C` (+ `\n`).
fn ascii_byte_filter(c: char, lcd_a02: bool) -> Option<char> {
    match c {
        '\n' => Some('\n'),
        '\\' if !lcd_a02 => Some('/'),
        '~' if !lcd_a02 => Some('-'),
        '\\' | '~' => Some(c),
        ' '..='}' => Some(c),
        '\t' => Some(' '),
        c if c.is_control() => None,
        _ => Some('?'),
    }
}

fn ascii_ok(c: char, lcd_a02: bool) -> bool {
    match c {
        '\n' => true,
        '\\' | '~' => lcd_a02,
        ' '..='}' => true,
        _ => false,
    }
}

/// Does this char need the per-grapheme slow path under the Utf8 tier?
fn utf8_needs_work(c: char, policy: EmojiPolicy) -> bool {
    match policy {
        EmojiPolicy::Mode2027 => false,
        EmojiPolicy::Strip => {
            c == VS16 || c == ZWJ || c == KEYCAP || is_fitzpatrick(c) || is_emoji_tag(c)
        }
        EmojiPolicy::Shortcode => {
            c == VS16
                || c == ZWJ
                || c == KEYCAP
                || is_fitzpatrick(c)
                || is_emoji_tag(c)
                || is_emoji_range(c)
        }
    }
}

/// Normalize text for display under the given context.
///
/// * `Utf8` + `Strip`: VS16, skin tones and emoji tags dropped, ZWJ sequences collapsed to their first
///   base, keycaps `X FE0F 20E3` → `[X]`, regional-indicator pairs kept.
/// * `Utf8` + `Shortcode`: emoji clusters → `:name:` (built-in table) or `:emoji:`.
/// * `Utf8` + `Mode2027`: unchanged.
/// * `VtSafe`: rich sigils absent from PSF fonts mapped (`⁴`→`[4]`, `◉`→`*`, `▶`→`>`, `✓`→`OK`,
///   `⟪`→`<<`), emoji → `:name:` or `?`.
/// * `Ascii7Bit`: output ⊂ `0x20..=0x7D \ {0x5C}` ∪ `{\n}` (A02 also allows `\` and `~`):
///   transliteration (accents, `ß`→`ss`, quotes, dashes, `…`, box/arrow glyphs), emoji → `:name:`
///   or `?`, everything else → `?`.
#[must_use]
pub fn normalize<'a>(s: &'a str, ctx: &TextCtx) -> Cow<'a, str> {
    match ctx.charset {
        ResolvedCharset::Utf8 => {
            if !s.chars().any(|c| utf8_needs_work(c, ctx.emoji)) {
                return Cow::Borrowed(s);
            }
            let mut out = String::with_capacity(s.len());
            for g in s.graphemes(true) {
                if is_emoji_cluster(g) {
                    match ctx.emoji {
                        EmojiPolicy::Mode2027 => out.push_str(g),
                        EmojiPolicy::Strip => emoji_strip(g, &mut out),
                        EmojiPolicy::Shortcode => emoji_shortcode(g, &mut out, ":emoji:"),
                    }
                } else if ctx.emoji == EmojiPolicy::Mode2027 {
                    out.push_str(g);
                } else {
                    // Text-presentation cluster: drop stray emoji modifiers only.
                    out.extend(
                        g.chars()
                            .filter(|&c| !(c == VS16 || is_fitzpatrick(c) || is_emoji_tag(c))),
                    );
                }
            }
            if out == s {
                Cow::Borrowed(s)
            } else {
                Cow::Owned(out)
            }
        }
        ResolvedCharset::VtSafe => {
            if s.chars()
                .all(|c| vtsafe_char(c).is_none() && !is_emoji_range(c) && !is_fitzpatrick(c))
            {
                return Cow::Borrowed(s);
            }
            let mut out = String::with_capacity(s.len());
            for g in s.graphemes(true) {
                let first = g.chars().next().unwrap_or(' ');
                if let Some(m) = vtsafe_char(first).filter(|_| !g.contains(KEYCAP)) {
                    out.push_str(m);
                    out.extend(
                        g.chars()
                            .skip(1)
                            .filter(|&c| vtsafe_char(c).is_none() && c != KEYCAP),
                    );
                } else if is_emoji_cluster(g) {
                    emoji_shortcode(g, &mut out, "?");
                } else {
                    out.extend(
                        g.chars()
                            .filter(|&c| !(is_fitzpatrick(c) || is_emoji_tag(c))),
                    );
                }
            }
            if out == s {
                Cow::Borrowed(s)
            } else {
                Cow::Owned(out)
            }
        }
        ResolvedCharset::Ascii7Bit => {
            if s.chars().all(|c| ascii_ok(c, ctx.lcd_a02)) {
                return Cow::Borrowed(s);
            }
            let mut tmp = String::with_capacity(s.len());
            for g in s.graphemes(true) {
                if is_emoji_cluster(g) {
                    emoji_shortcode(g, &mut tmp, "?");
                } else {
                    for c in g.chars() {
                        ascii_translit(c, &mut tmp);
                    }
                }
            }
            Cow::Owned(
                tmp.chars()
                    .filter_map(|c| ascii_byte_filter(c, ctx.lcd_a02))
                    .collect(),
            )
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Truncate / fit
// ---------------------------------------------------------------------------------------------

/// Truncate to at most `max` cells, appending `ellipsis` when truncated.
#[must_use]
pub fn truncate(s: &str, max: usize, ellipsis: &str) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    let ew = str_width(ellipsis);
    if max < ew {
        return fit(s, max).trim_end().to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = grapheme_width(g);
        if w + gw > max - ew {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push_str(ellipsis);
    out
}

/// Exactly `w` cells: truncate without ellipsis and pad with spaces (a wide glyph that would
/// cross the edge becomes padding).
#[must_use]
pub fn fit(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for g in s.graphemes(true) {
        let gw = grapheme_width(g);
        if used + gw > w {
            break;
        }
        out.push_str(g);
        used += gw;
    }
    while used < w {
        out.push(' ');
        used += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Wrapping
// ---------------------------------------------------------------------------------------------

/// One wrapped visual line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    /// Line text.
    pub text: String,
    /// Grapheme index (in the source string, `\n` counting as one) of the first cluster.
    pub start_grapheme: u32,
    /// Display width.
    pub width: u16,
}

/// Core greedy wrapper over one logical line (no newlines) given per-grapheme widths.
///
/// Returns half-open grapheme-index ranges, one per visual line (always ≥ 1).
///
/// * `hard == false`: word-wrap at `' '`; the spaces at a break are consumed (they belong to no
///   line); leading indentation of the logical line is kept; overlong words hard-break.
/// * `hard == true`: break purely by width, keeping every grapheme (code blocks).
///
/// A grapheme wider than `width` (only possible when `width == 1`) is placed alone on its line;
/// callers render it as `' '` (I4).
#[must_use]
pub fn wrap_ranges(
    graphemes: &[&str],
    widths: &[usize],
    width: usize,
    hard: bool,
) -> Vec<Range<usize>> {
    let width = width.max(1);
    let n = graphemes.len().min(widths.len());
    let mut out = Vec::new();
    if n == 0 {
        out.push(0..0);
        return out;
    }
    let cell = |i: usize| widths[i].min(width);

    if hard {
        let mut start = 0;
        let mut w = 0;
        for i in 0..n {
            let gw = cell(i);
            if w + gw > width && i > start {
                out.push(start..i);
                start = i;
                w = 0;
            }
            w += gw;
        }
        out.push(start..n);
        return out;
    }

    // Line under construction: [start, end) with width w. `None` start = no line open yet.
    let mut start: Option<usize> = None;
    let mut end = 0usize;
    let mut w = 0usize;
    let mut pending: Option<Range<usize>> = None; // spaces seen since the last word
    let mut i = 0usize;

    // Hard-break a word [a, b) starting on a fresh or partially-filled line.
    let place_word = |a: usize,
                      b: usize,
                      start: &mut Option<usize>,
                      end: &mut usize,
                      w: &mut usize,
                      out: &mut Vec<Range<usize>>| {
        for j in a..b {
            let gw = cell(j);
            match *start {
                None => {
                    *start = Some(j);
                    *w = 0;
                }
                Some(s) if *w + gw > width && *end > s => {
                    out.push(s..*end);
                    *start = Some(j);
                    *w = 0;
                }
                Some(_) => {}
            }
            *w += gw;
            *end = j + 1;
        }
    };

    while i < n {
        let is_space = graphemes[i] == " ";
        let mut j = i;
        while j < n && (graphemes[j] == " ") == is_space {
            j += 1;
        }
        if is_space {
            if i == 0 {
                // Leading indentation: content, not a break opportunity.
                place_word(i, j, &mut start, &mut end, &mut w, &mut out);
            } else {
                pending = Some(i..j);
            }
        } else {
            let ww: usize = (i..j).map(cell).sum();
            let sw = pending.as_ref().map_or(0, ExactSizeIterator::len);
            match start {
                Some(s) if w + sw + ww <= width => {
                    w += sw + ww;
                    end = j;
                    let _ = s;
                }
                Some(s) if pending.is_none() => {
                    // Glued to previous content (e.g. after indentation) — hard-break in place.
                    let _ = s;
                    place_word(i, j, &mut start, &mut end, &mut w, &mut out);
                }
                Some(s) => {
                    out.push(s..end);
                    start = None;
                    w = 0;
                    place_word(i, j, &mut start, &mut end, &mut w, &mut out);
                }
                None => place_word(i, j, &mut start, &mut end, &mut w, &mut out),
            }
            pending = None;
        }
        i = j;
    }
    // Trailing spaces: keep the ones that still fit, drop the rest (never open a line of blanks).
    if let Some(p) = pending {
        match start {
            Some(_) => {
                for k in p {
                    if w + cell(k) > width {
                        break;
                    }
                    w += cell(k);
                    end = k + 1;
                }
            }
            None => {
                // Only possible if the line is all spaces after… nothing; treat as indentation.
                place_word(p.start, p.end, &mut start, &mut end, &mut w, &mut out);
            }
        }
    }
    match start {
        Some(s) => out.push(s..end),
        None => out.push(n..n),
    }
    out
}

fn is_newline_cluster(g: &str) -> bool {
    g.contains('\n')
}

/// Word-wrap into lines of at most `width` cells (always at least one line).
///
/// `\n` is a hard break (counts as one grapheme for `start_grapheme`), clusters are never split,
/// and a 2-cell cluster never crosses `width` (at `width == 1` it is rendered as `' '`).
#[must_use]
pub fn wrap(s: &str, width: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let all: Vec<&str> = s.graphemes(true).collect();
    let mut lines = Vec::new();
    let mut base = 0usize;
    for logical in all.split(|g| is_newline_cluster(g)) {
        let widths: Vec<usize> = logical.iter().map(|g| grapheme_width(g)).collect();
        for r in wrap_ranges(logical, &widths, width, false) {
            let mut text = String::new();
            let mut w = 0usize;
            for k in r.clone() {
                if widths[k] > width {
                    text.push(' ');
                    w += 1;
                } else {
                    text.push_str(logical[k]);
                    w += widths[k];
                }
            }
            lines.push(WrappedLine {
                text,
                start_grapheme: u32::try_from(base + r.start).unwrap_or(u32::MAX),
                width: u16::try_from(w).unwrap_or(u16::MAX),
            });
        }
        base += logical.len() + 1;
    }
    lines
}

// ---------------------------------------------------------------------------------------------
// Buffer writing (I4)
// ---------------------------------------------------------------------------------------------

/// Write `s` at `(x, y)` clipped to `max_w` cells and the buffer area; never splits a wide
/// grapheme across the clip edge (pads with a space instead). Returns cells written.
///
/// Safe for any `x`/`y`: columns left of the buffer are skipped (a wide glyph straddling the left
/// edge becomes `' '`), rows outside the buffer write nothing. The return value is the number of
/// columns consumed from `x` (including a padding space), or 0 if the row or start column lies
/// outside the buffer.
pub fn put_str_clipped(buf: &mut Buffer, x: u16, y: u16, s: &str, max_w: u16, style: Style) -> u16 {
    let area = buf.area;
    if y < area.y || y >= area.bottom() || x >= area.right() || area.width == 0 {
        return 0;
    }
    let end = (u32::from(x) + u32::from(max_w)).min(u32::from(area.right()));
    let left = u32::from(area.x);
    let mut col = u32::from(x);
    let set = |buf: &mut Buffer, c: u32, sym: &str| {
        if c >= left
            && c < end
            && let Ok(cx) = u16::try_from(c)
        {
            buf[(cx, y)].set_symbol(sym).set_style(style);
        }
    };
    for g in s.graphemes(true) {
        let gw = grapheme_width(g) as u32;
        if gw == 0 {
            continue;
        }
        if col >= end {
            break;
        }
        if col + gw > end {
            // Wide glyph would cross the right clip edge: pad instead (I4).
            for c in col..end {
                set(buf, c, " ");
            }
            col = end;
            break;
        }
        if col + gw <= left {
            col += gw;
            continue;
        }
        if col < left {
            // Straddles the left buffer edge: the visible half becomes a space.
            for c in col..col + gw {
                set(buf, c, " ");
            }
        } else {
            set(buf, col, g);
            for c in col + 1..col + gw {
                if c < end
                    && let Ok(cx) = u16::try_from(c)
                {
                    let cell = &mut buf[(cx, y)];
                    cell.reset();
                    cell.set_style(style);
                }
            }
        }
        col += gw;
    }
    u16::try_from(col - u32::from(x)).unwrap_or(u16::MAX)
}

/// Blank orphaned wide-glyph halves along the left/right edges of `area` (I4).
///
/// For every row of `area` (intersected with the buffer):
/// * if the cell just left of `area.x` holds a wide glyph (its second half is inside `area`,
///   i.e. it was overwritten), that glyph is replaced by `' '`;
/// * if the cell at `area.right() - 1` holds a wide glyph (its second half lies outside), that
///   glyph is replaced by `' '`.
pub fn blank_orphans(buf: &mut Buffer, area: Rect) {
    let area = area.intersection(buf.area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    for y in area.top()..area.bottom() {
        if area.x > buf.area.x {
            let cell = &mut buf[(area.x - 1, y)];
            if grapheme_width(cell.symbol()) >= 2 {
                cell.set_symbol(" ");
            }
        }
        let rx = area.right() - 1;
        let cell = &mut buf[(rx, y)];
        if grapheme_width(cell.symbol()) >= 2 {
            cell.set_symbol(" ");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(charset: ResolvedCharset) -> TextCtx {
        TextCtx::new(charset)
    }

    #[test]
    fn widths() {
        assert_eq!(grapheme_width("a"), 1);
        assert_eq!(grapheme_width("\n"), 0);
        assert_eq!(grapheme_width("\u{1b}"), 0);
        assert_eq!(grapheme_width("\u{200B}"), 0);
        assert_eq!(grapheme_width("中"), 2);
        assert_eq!(grapheme_width("😀"), 2);
        assert_eq!(grapheme_width("❤"), 1);
        assert_eq!(grapheme_width("❤\u{FE0F}"), 2);
        assert_eq!(grapheme_width("1\u{FE0F}\u{20E3}"), 2);
        assert_eq!(grapheme_width("🇺🇸"), 2);
        assert_eq!(grapheme_width("👨\u{200D}👩\u{200D}👧"), 2);
        assert_eq!(grapheme_width("👍🏽"), 2);
        assert_eq!(grapheme_width("e\u{301}"), 1);
        assert_eq!(str_width("ab中😀"), 6);
    }

    #[test]
    fn strip_policy() {
        let c = ctx(ResolvedCharset::Utf8);
        assert_eq!(normalize("hi", &c), "hi");
        assert!(matches!(normalize("plain 中文", &c), Cow::Borrowed(_)));
        assert_eq!(normalize("❤\u{FE0F}", &c), "❤");
        assert_eq!(normalize("👍🏽", &c), "👍");
        assert_eq!(normalize("👨\u{200D}👩\u{200D}👧!", &c), "👨!");
        assert_eq!(normalize("1\u{FE0F}\u{20E3}", &c), "[1]");
        assert_eq!(normalize("🇺🇸", &c), "🇺🇸");
        assert_eq!(
            normalize(
                "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}",
                &c
            ),
            "🏴"
        );
        let m = TextCtx {
            emoji: EmojiPolicy::Mode2027,
            ..c
        };
        assert_eq!(normalize("👍🏽", &m), "👍🏽");
    }

    #[test]
    fn shortcode_policy() {
        let c = TextCtx {
            emoji: EmojiPolicy::Shortcode,
            ..ctx(ResolvedCharset::Utf8)
        };
        assert_eq!(normalize("ok 👍🏽!", &c), "ok :thumbsup:!");
        assert_eq!(normalize("🦩", &c), ":emoji:");
        assert_eq!(normalize("🇺🇸", &c), ":flag_us:");
        assert_eq!(normalize("✓ done ❤", &c), "✓ done ❤");
        assert_eq!(normalize("❤\u{FE0F}", &c), ":heart:");
    }

    #[test]
    fn vtsafe() {
        let c = ctx(ResolvedCharset::VtSafe);
        assert_eq!(normalize("⁴⁵⁶ ◉ ▶ ✓ ⟪RLO⟫", &c), "[4][5][6] * > OK <<RLO>>");
        assert_eq!(normalize("¹²³ ┌─┐ é", &c), "¹²³ ┌─┐ é");
        assert_eq!(normalize("hi 😀 🦩", &c), "hi :grinning: ?");
    }

    #[test]
    fn ascii_translit_table() {
        let c = ctx(ResolvedCharset::Ascii7Bit);
        assert_eq!(normalize("Café straße", &c), "Cafe strasse");
        assert_eq!(normalize("“quote” ‘x’ — … ", &c), "\"quote\" 'x' - ... ");
        assert_eq!(normalize("Łódź Œuvre", &c), "Lodz OEuvre");
        assert_eq!(normalize("中文", &c), "??");
        assert_eq!(normalize("a\\b~c", &c), "a/b-c");
        assert_eq!(normalize("╔═╗│→", &c), "+=+|->");
        assert_eq!(normalize("😀 1\u{FE0F}\u{20E3}", &c), ":grinning: [1]");
        assert_eq!(normalize("e\u{301}", &c), "e");
        let a02 = TextCtx { lcd_a02: true, ..c };
        assert_eq!(normalize("a\\b~c", &a02), "a\\b~c");
    }

    #[test]
    fn ascii_byte_set_property() {
        let c = ctx(ResolvedCharset::Ascii7Bit);
        let mut sample = String::new();
        for cp in (0u32..0x3000).chain(0x1F300..0x1F700).chain(0xFF00..0xFFFF) {
            if let Some(ch) = char::from_u32(cp) {
                sample.push(ch);
            }
        }
        sample.push_str("👨\u{200D}👩\u{200D}👧 🇺🇸 1\u{FE0F}\u{20E3} \u{1b}]8;;evil\u{7}");
        let out = normalize(&sample, &c);
        for b in out.bytes() {
            assert!(
                b == b'\n' || ((0x20..=0x7D).contains(&b) && b != 0x5C),
                "byte {b:#x}"
            );
        }
    }

    fn check_wrap(s: &str, width: usize) {
        let lines = wrap(s, width);
        assert!(!lines.is_empty());
        let src: Vec<&str> = s.graphemes(true).collect();
        let mut prev: Option<u32> = None;
        let mut covered = String::new();
        for l in &lines {
            assert!(usize::from(l.width) <= width, "{s:?} @ {width}: {l:?}");
            assert_eq!(usize::from(l.width), str_width(&l.text));
            if let Some(p) = prev {
                assert!(
                    l.start_grapheme > p || (l.start_grapheme == p && l.text.is_empty()),
                    "{lines:?}"
                );
            }
            prev = Some(l.start_grapheme);
            // The line's graphemes are a contiguous run of the source starting at start_grapheme.
            let start = l.start_grapheme as usize;
            for (k, g) in l.text.graphemes(true).enumerate() {
                let sg = src[start + k];
                assert!(
                    g == sg || (g == " " && grapheme_width(sg) > width),
                    "split cluster {g:?} vs {sg:?}"
                );
            }
            covered.push_str(&l.text);
        }
        let strip = |t: &str| {
            t.chars()
                .filter(|c| *c != ' ' && *c != '\n')
                .collect::<String>()
        };
        if width >= 2 {
            assert_eq!(strip(&covered), strip(s), "coverage at {width}");
        }
    }

    #[test]
    fn wrap_cjk_emoji_odd_widths() {
        let samples = [
            "中文字符测试一下换行是否正确处理",
            "a中b文c字d符 e😀f 👍🏽 🇺🇸🇯🇵 x",
            "hello world, this is a fairly long sentence with words",
            "supercalifragilisticexpialidocious word",
            "  indented line\nsecond\n\nfourth   ",
            "e\u{301}e\u{301}e\u{301} combining",
            "",
            "   ",
        ];
        for s in samples {
            for w in 1..=80 {
                check_wrap(s, w);
            }
        }
    }

    #[test]
    fn wrap_basics() {
        let l = wrap("hello world foo", 11);
        assert_eq!(
            l.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["hello world", "foo"]
        );
        assert_eq!(l[1].start_grapheme, 12);
        let l = wrap("ab\ncd", 10);
        assert_eq!(l.len(), 2);
        assert_eq!(l[1].start_grapheme, 3);
        let l = wrap("abcdef", 4);
        assert_eq!(
            l.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["abcd", "ef"]
        );
        let l = wrap("a中", 2);
        assert_eq!(
            l.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["a", "中"]
        );
        let l = wrap("中", 1);
        assert_eq!(l[0].text, " ");
        assert_eq!(wrap("", 5).len(), 1);
    }

    #[test]
    fn wrap_anchor_stable_across_widths() {
        let s = "The quick brown 狐狸 jumps over the lazy 犬. 😀 Again and again\nand a second paragraph here.";
        let n = s.graphemes(true).count() as u32;
        for anchor in 0..n {
            for w in 1..=80 {
                let lines = wrap(s, w);
                let idx = lines.iter().rposition(|l| l.start_grapheme <= anchor);
                assert!(idx.is_some());
            }
        }
    }

    #[test]
    fn truncate_fit() {
        assert_eq!(truncate("hello world", 8, "…"), "hello w…");
        assert_eq!(str_width(&truncate("中文中文中文", 5, "…")), 5);
        assert_eq!(fit("中文", 3), "中 ");
        assert_eq!(fit("ab", 4), "ab  ");
    }

    #[test]
    fn put_str_edges() {
        let mut buf = Buffer::empty(Rect::new(2, 1, 6, 2));
        // Right edge: wide glyph crossing → space.
        let n = put_str_clipped(&mut buf, 6, 1, "a中", 10, Style::default());
        assert_eq!(n, 2);
        assert_eq!(buf[(6, 1)].symbol(), "a");
        assert_eq!(buf[(7, 1)].symbol(), " ");
        // max_w clip.
        put_str_clipped(&mut buf, 2, 2, "中中", 3, Style::default());
        assert_eq!(buf[(2, 2)].symbol(), "中");
        assert_eq!(buf[(4, 2)].symbol(), " ");
        assert_eq!(buf[(5, 2)].symbol(), " ");
        // Left of the buffer: straddling wide glyph → space.
        let mut buf = Buffer::empty(Rect::new(2, 0, 4, 1));
        put_str_clipped(&mut buf, 1, 0, "中b", 10, Style::default());
        assert_eq!(buf[(2, 0)].symbol(), " ");
        assert_eq!(buf[(3, 0)].symbol(), "b");
        // Out of range never panics.
        for x in [0, 1, 5, 6, 100, u16::MAX] {
            for y in [0, 1, 2, u16::MAX] {
                let _ = put_str_clipped(&mut buf, x, y, "中x😀", u16::MAX, Style::default());
            }
        }
        let mut empty = Buffer::empty(Rect::new(0, 0, 0, 0));
        assert_eq!(
            put_str_clipped(&mut empty, 0, 0, "x", 5, Style::default()),
            0
        );
    }

    #[test]
    fn orphans() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        put_str_clipped(&mut buf, 0, 0, "a中bcd中", 10, Style::default());
        // "a中bcd中" occupies 0..8; area 2..7 splits 中 at 1..3 (left) and 中 at 6..8 (right).
        blank_orphans(&mut buf, Rect::new(2, 0, 5, 1));
        assert_eq!(buf[(1, 0)].symbol(), " ");
        assert_eq!(buf[(6, 0)].symbol(), " ");
        assert_eq!(buf[(3, 0)].symbol(), "b");
        blank_orphans(&mut buf, Rect::new(50, 50, 5, 5));
    }
}
