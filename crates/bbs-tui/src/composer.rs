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
