//! Remote-content sanitizer (§4.6).
//!
//! Every remote string (content, names, embeds, filenames, URLs, topics, paste) passes through
//! here before it can reach the PTY (I6):
//!
//! * C0 controls except `\n` are removed (`\t` is expanded to spaces up to the next multiple of 4
//!   cells relative to the line start), as are `DEL`, C1 `U+0080..=U+009F` and therefore `ESC`,
//!   CSI (`U+009B`), OSC (`U+009D`) and ST (`U+009C`): no escape sequence can survive.
//! * Bidi embeddings/overrides/isolates (`U+202A..=U+202E`, `U+2066..=U+2069`, `U+061C`) are made
//!   visible as `⟪RLO⟫` etc. instead of being interpreted (Trojan-Source style spoofing).
//!   `U+200E`/`U+200F` marks are dropped. `U+2028`/`U+2029` become `\n`.
//! * OSC 8 URLs are validated separately by [`sanitize_osc8_url`].

use std::borrow::Cow;

use unicode_width::UnicodeWidthChar;

/// Visible replacement for a bidi control, if `c` is one.
const fn bidi_marker(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{202A}' => "⟪LRE⟫",
        '\u{202B}' => "⟪RLE⟫",
        '\u{202C}' => "⟪PDF⟫",
        '\u{202D}' => "⟪LRO⟫",
        '\u{202E}' => "⟪RLO⟫",
        '\u{2066}' => "⟪LRI⟫",
        '\u{2067}' => "⟪RLI⟫",
        '\u{2068}' => "⟪FSI⟫",
        '\u{2069}' => "⟪PDI⟫",
        '\u{061C}' => "⟪ALM⟫",
        _ => return None,
    })
}

/// Does `c` need rewriting by the sanitizer?
fn needs_work(c: char) -> bool {
    (c.is_control() && c != '\n')
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}')
        || bidi_marker(c).is_some()
}

/// Sanitize multi-line remote text (content, embed descriptions, paste).
#[must_use]
pub fn sanitize_remote(s: &str) -> Cow<'_, str> {
    if !s.chars().any(needs_work) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0usize; // display column since the last '\n' (for tab stops)
    for c in s.chars() {
        match c {
            '\n' | '\u{2028}' | '\u{2029}' => {
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let n = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\u{200E}' | '\u{200F}' => {}
            c if c.is_control() => {}
            c => {
                if let Some(m) = bidi_marker(c) {
                    out.push_str(m);
                    col += m
                        .chars()
                        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                        .sum::<usize>();
                } else {
                    out.push(c);
                    col += UnicodeWidthChar::width(c).unwrap_or(0);
                }
            }
        }
    }
    Cow::Owned(out)
}

/// Sanitize single-line remote text (names, titles, topics): newlines become spaces.
#[must_use]
pub fn sanitize_single_line(s: &str) -> Cow<'_, str> {
    let c = sanitize_remote(s);
    if c.contains('\n') {
        Cow::Owned(c.replace('\n', " "))
    } else {
        c
    }
}

/// Maximum accepted length of an encoded OSC 8 URL.
pub const OSC8_URL_MAX: usize = 2048;

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Validate and percent-encode a URL for OSC 8; `None` when it must not be linked.
///
/// Only `http://` / `https://` (scheme case-insensitive) with a non-empty remainder are accepted.
/// URLs whose percent-decoded form contains `ESC`, `BEL` or `ST` (`0x9C` or `U+009C`) are
/// rejected. Every byte `≤ 0x20`, `0x7F..=0x9F`, `;` and every non-ASCII byte is percent-encoded
/// (existing `%XX` escapes are kept). Results longer than [`OSC8_URL_MAX`] are rejected.
#[must_use]
pub fn sanitize_osc8_url(url: &str) -> Option<String> {
    let lower_prefix: String = url.chars().take(8).collect::<String>().to_ascii_lowercase();
    let rest_at = if lower_prefix.starts_with("https://") {
        8
    } else if lower_prefix.starts_with("http://") {
        7
    } else {
        return None;
    };
    if url.len() <= rest_at {
        return None;
    }
    let decoded = percent_decode(url);
    if decoded.iter().any(|&b| matches!(b, 0x1B | 0x07 | 0x9C)) {
        return None;
    }
    if String::from_utf8_lossy(&decoded).contains('\u{9C}') {
        return None;
    }
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..rest_at].to_ascii_lowercase());
    for &b in &url.as_bytes()[rest_at..] {
        if b <= 0x20 || (0x7F..=0x9F).contains(&b) || b == b';' || b >= 0x80 {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b & 0xF)]));
        } else {
            out.push(char::from(b));
        }
        if out.len() > OSC8_URL_MAX {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_borrowed() {
        assert!(matches!(
            sanitize_remote("hello\nworld 中 😀"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn control_table() {
        let cases: &[(&str, &str)] = &[
            ("a\u{1b}[31mred", "a[31mred"),
            (
                "\u{1b}]8;;evil\u{7}click\u{1b}]8;;\u{7}",
                "]8;;evilclick]8;;",
            ),
            ("x\u{9b}2Jy", "x2Jy"),
            ("x\u{9d}0;title\u{9c}y", "x0;titley"),
            ("bell\u{7}\u{8}\u{0}\u{7f}", "bell"),
            ("cr\r\nlf", "cr\nlf"),
            ("\u{85}nel", "nel"),
            ("\tx", "    x"),
            ("ab\tc", "ab  c"),
            ("abcd\te", "abcd    e"),
            ("a\nb\tc", "a\nb   c"),
            ("中\tx", "中  x"),
            ("evil\u{202E}txt.exe", "evil⟪RLO⟫txt.exe"),
            (
                "\u{2066}\u{2067}\u{2068}\u{2069}\u{061C}",
                "⟪LRI⟫⟪RLI⟫⟪FSI⟫⟪PDI⟫⟪ALM⟫",
            ),
            ("\u{202A}\u{202B}\u{202C}\u{202D}", "⟪LRE⟫⟪RLE⟫⟪PDF⟫⟪LRO⟫"),
            ("a\u{200E}b\u{200F}c", "abc"),
            ("l1\u{2028}l2", "l1\nl2"),
        ];
        for (inp, want) in cases {
            assert_eq!(sanitize_remote(inp), *want, "input {inp:?}");
        }
    }

    #[test]
    fn fuzzish_no_controls_survive() {
        let mut s = String::new();
        for cp in 0u32..0x300 {
            if let Some(c) = char::from_u32(cp) {
                s.push(c);
            }
        }
        s.push_str("\u{202E}\u{2066}\u{200F}\u{1b}P1;2q\u{1b}\\\u{1b}_G\u{1b}\\");
        let out = sanitize_remote(&s);
        assert!(out.chars().all(|c| c == '\n' || !c.is_control()));
        assert!(!out.chars().any(|c| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}')));
        let one = sanitize_single_line("a\nb\u{1b}c");
        assert_eq!(one, "a bc");
    }

    #[test]
    fn osc8() {
        assert_eq!(
            sanitize_osc8_url("https://example.com/a b").as_deref(),
            Some("https://example.com/a%20b")
        );
        assert_eq!(
            sanitize_osc8_url("HTTP://x.y/;q").as_deref(),
            Some("http://x.y/%3Bq")
        );
        assert_eq!(
            sanitize_osc8_url("https://ü.de/é").as_deref(),
            Some("https://%C3%BC.de/%C3%A9")
        );
        assert_eq!(
            sanitize_osc8_url("https://x.y/%41").as_deref(),
            Some("https://x.y/%41")
        );
        assert_eq!(sanitize_osc8_url("javascript:alert(1)"), None);
        assert_eq!(sanitize_osc8_url("file:///etc/passwd"), None);
        assert_eq!(sanitize_osc8_url("https://"), None);
        assert_eq!(sanitize_osc8_url("https://x.y/\u{1b}]8;;"), None);
        assert_eq!(sanitize_osc8_url("https://x.y/%1b]8;;evil%07"), None);
        assert_eq!(sanitize_osc8_url("https://x.y/%07"), None);
        assert_eq!(sanitize_osc8_url("https://x.y/%9c"), None);
        assert_eq!(sanitize_osc8_url("https://x.y/%C2%9C"), None);
        assert_eq!(sanitize_osc8_url("https://x.y/\u{9c}"), None);
        assert_eq!(
            sanitize_osc8_url("https://x.y/%"),
            Some("https://x.y/%".to_string())
        );
        let long = format!("https://x.y/{}", "a".repeat(3000));
        assert_eq!(sanitize_osc8_url(&long), None);
        let ok = sanitize_osc8_url("https://x.y/\u{7f}\u{85}\n").unwrap_or_default();
        assert_eq!(ok, "https://x.y/%7F%C2%85%0A");
        assert!(ok.bytes().all(|b| b > 0x20 && b < 0x7F && b != b';'));
    }
}
