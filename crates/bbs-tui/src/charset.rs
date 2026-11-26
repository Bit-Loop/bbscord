//! Glyph tables per charset tier and frame style (§1.3, §7).

use bbs_core::ResolvedCharset;
use serde::{Deserialize, Serialize};

/// User-selectable frame style (Utf8Rich only; lower tiers are forced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameStyle {
    /// `╔═╗` CP437 double (default, BBS soul).
    #[default]
    Double,
    /// `┌─┐`.
    Single,
    /// `╭─╮` (btop default).
    Rounded,
    /// `┏━┓`.
    Heavy,
    /// `+-+`.
    Ascii,
}

/// All glyphs the chrome and widgets draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Top-left corner.
    pub tl: &'static str,
    /// Top-right corner.
    pub tr: &'static str,
    /// Bottom-left corner.
    pub bl: &'static str,
    /// Bottom-right corner.
    pub br: &'static str,
    /// Outer horizontal.
    pub h: &'static str,
    /// Outer / divider vertical.
    pub v: &'static str,
    /// Divider meets top border.
    pub t_down: &'static str,
    /// Divider meets bottom border.
    pub t_up: &'static str,
    /// Separator row left junction (on a vertical line).
    pub sep_l: &'static str,
    /// Separator row right junction.
    pub sep_r: &'static str,
    /// Inner (single) horizontal rule.
    pub rule: &'static str,
    /// Inner (single) vertical divider (Short chrome).
    pub thin_v: &'static str,
    /// Title opening bracket inside a border.
    pub title_l: &'static str,
    /// Title closing bracket inside a border.
    pub title_r: &'static str,
    /// Truncation ellipsis.
    pub ellipsis: &'static str,
    /// Selection / focus marker.
    pub cursor: &'static str,
    /// Unread dot.
    pub unread: &'static str,
    /// Speaking indicator.
    pub speaking: &'static str,
    /// Silent participant indicator.
    pub silent: &'static str,
    /// Reply arrow.
    pub reply: &'static str,
    /// Play glyph.
    pub play: &'static str,
    /// Pause glyph.
    pub pause: &'static str,
    /// Verified check.
    pub check: &'static str,
    /// DAVE unverified mark.
    pub unverified: &'static str,
    /// Scrollbar thumb.
    pub thumb: &'static str,
    /// Scrollbar track.
    pub track: &'static str,
    /// Embed colour bar.
    pub embed_bar: &'static str,
    /// Attachment marker.
    pub clip: &'static str,
    /// Telephone / ring.
    pub phone: &'static str,
    /// Middle dot separator.
    pub dot: &'static str,
    /// Collapsed tree node.
    pub collapsed: &'static str,
    /// Expanded tree node.
    pub expanded: &'static str,
    /// Superscript-capable pane numbers.
    pub superscripts: bool,
    /// Tier.
    pub tier: ResolvedCharset,
}

const RICH_COMMON: Glyphs = Glyphs {
    tl: "╔",
    tr: "╗",
    bl: "╚",
    br: "╝",
    h: "═",
    v: "║",
    t_down: "╦",
    t_up: "╩",
    sep_l: "╟",
    sep_r: "╢",
    rule: "─",
    thin_v: "│",
    title_l: "╡",
    title_r: "╞",
    ellipsis: "…",
    cursor: "▶",
    unread: "●",
    speaking: "◉",
    silent: "○",
    reply: "↩",
    play: "▶",
    pause: "⏸",
    check: "✓",
    unverified: "~",
    thumb: "┃",
    track: "│",
    embed_bar: "▌",
    clip: "📎",
    phone: "☎",
    dot: "·",
    collapsed: "▸",
    expanded: "▾",
    superscripts: true,
    tier: ResolvedCharset::Utf8,
};

impl Glyphs {
    /// Glyph table for a tier and (Utf8Rich-only) frame style.
    #[must_use]
    pub const fn for_tier(tier: ResolvedCharset, style: FrameStyle) -> Self {
        match tier {
            ResolvedCharset::Utf8 => match style {
                FrameStyle::Double => RICH_COMMON,
                FrameStyle::Single => Self {
                    tl: "┌",
                    tr: "┐",
                    bl: "└",
                    br: "┘",
                    h: "─",
                    v: "│",
                    t_down: "┬",
                    t_up: "┴",
                    sep_l: "├",
                    sep_r: "┤",
                    title_l: "┤",
                    title_r: "├",
                    ..RICH_COMMON
                },
                FrameStyle::Rounded => Self {
                    tl: "╭",
                    tr: "╮",
                    bl: "╰",
                    br: "╯",
                    h: "─",
                    v: "│",
                    t_down: "┬",
                    t_up: "┴",
                    sep_l: "├",
                    sep_r: "┤",
                    title_l: "┤",
                    title_r: "├",
                    ..RICH_COMMON
                },
                FrameStyle::Heavy => Self {
                    tl: "┏",
                    tr: "┓",
                    bl: "┗",
                    br: "┛",
                    h: "━",
                    v: "┃",
                    t_down: "┳",
                    t_up: "┻",
                    sep_l: "┠",
                    sep_r: "┨",
                    title_l: "┫",
                    title_r: "┣",
                    ..RICH_COMMON
                },
                FrameStyle::Ascii => Self::ascii(),
            },
            ResolvedCharset::VtSafe => Self {
                sep_l: "├",
                sep_r: "┤",
                title_l: "[",
                title_r: "]",
                ellipsis: ".",
                cursor: ">",
                unread: "*",
                speaking: "*",
                silent: "o",
                reply: "<",
                play: ">",
                pause: "=",
                check: "OK",
                unverified: "?",
                thumb: "█",
                track: "░",
                embed_bar: "▌",
                clip: "+",
                phone: "!",
                dot: "-",
                collapsed: "+",
                expanded: "-",
                superscripts: false,
                tier: ResolvedCharset::VtSafe,
                ..RICH_COMMON
            },
            ResolvedCharset::Ascii7Bit => Self::ascii(),
        }
    }

    const fn ascii() -> Self {
        Self {
            tl: "+",
            tr: "+",
            bl: "+",
            br: "+",
            h: "=",
            v: "|",
            t_down: "+",
            t_up: "+",
            sep_l: "+",
            sep_r: "+",
            rule: "-",
            thin_v: "|",
            title_l: "[",
            title_r: "]",
            ellipsis: ".",
            cursor: ">",
            unread: "*",
            speaking: "*",
            silent: "o",
            reply: "<",
            play: ">",
            pause: "=",
            check: "OK",
            unverified: "?",
            thumb: "#",
            track: "|",
            embed_bar: "|",
            clip: "+",
            phone: "!",
            dot: "-",
            collapsed: "+",
            expanded: "-",
            superscripts: false,
            tier: ResolvedCharset::Ascii7Bit,
        }
    }

    /// Pane number label: `¹` (rich) or `[1]`.
    #[must_use]
    pub fn pane_number(&self, n: u8) -> String {
        const SUP: [&str; 10] = ["⁰", "¹", "²", "³", "⁴", "⁵", "⁶", "⁷", "⁸", "⁹"];
        if self.superscripts {
            SUP[usize::from(n % 10)].to_string()
        } else {
            format!("[{}]", n % 10)
        }
    }

    /// Whether this is the strict 7-bit tier.
    #[must_use]
    pub const fn is_ascii(&self) -> bool {
        matches!(self.tier, ResolvedCharset::Ascii7Bit)
    }
}

/// Assert a string is HD44780-A00 safe (`0x20..=0x7D` minus `0x5C`).
#[must_use]
pub fn is_a00_safe(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != 0x5C)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_glyphs_are_a00_safe() {
        let g = Glyphs::for_tier(ResolvedCharset::Ascii7Bit, FrameStyle::Double);
        for s in [
            g.tl,
            g.tr,
            g.bl,
            g.br,
            g.h,
            g.v,
            g.t_down,
            g.t_up,
            g.sep_l,
            g.sep_r,
            g.rule,
            g.thin_v,
            g.title_l,
            g.title_r,
            g.ellipsis,
            g.cursor,
            g.unread,
            g.speaking,
            g.silent,
            g.reply,
            g.play,
            g.pause,
            g.check,
            g.unverified,
            g.thumb,
            g.track,
            g.embed_bar,
            g.clip,
            g.phone,
            g.dot,
            g.collapsed,
            g.expanded,
        ] {
            assert!(is_a00_safe(s), "{s:?}");
        }
        assert_eq!(g.pane_number(3), "[3]");
    }

    #[test]
    fn rich_superscripts() {
        let g = Glyphs::for_tier(ResolvedCharset::Utf8, FrameStyle::Double);
        assert_eq!(g.pane_number(2), "²");
    }
}
