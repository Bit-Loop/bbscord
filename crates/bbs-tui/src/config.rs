//! TUI configuration resolved from terminal capabilities, environment and user settings.

use bbs_core::{
    CharsetMode, ColorDepth, GraphicsTier, KeyboardProtocol, MultiplexerKind, ResolvedCharset,
    TerminalCapabilities,
};
use serde::{Deserialize, Serialize};

use crate::charset::FrameStyle;
use crate::text::EmojiPolicy;

/// `Ctrl+K` behaviour in the composer (§9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CtrlKMode {
    /// Switcher at end of buffer / empty composer, kill-to-EOL elsewhere.
    #[default]
    Dwim,
    /// Always readline kill-to-EOL.
    Kill,
    /// Always quick switcher.
    Switcher,
}

/// Resolved runtime configuration for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiConfig {
    /// Charset tier.
    pub charset: ResolvedCharset,
    /// Colour depth.
    pub depth: ColorDepth,
    /// Emoji width policy.
    pub emoji: EmojiPolicy,
    /// Frame style (Utf8Rich only).
    pub frame: FrameStyle,
    /// Vertical positional border gradients.
    pub gradient_borders: bool,
    /// HD44780 A02 ROM.
    pub lcd_a02: bool,
    /// Ctrl+K mode.
    pub ctrl_k: CtrlKMode,
    /// Micro auto-read speed.
    pub wpm: u16,
    /// Low-bandwidth mode (§11).
    pub low_bandwidth: bool,
    /// Graphics tier.
    pub graphics: GraphicsTier,
    /// Running over SSH.
    pub over_ssh: bool,
    /// Multiplexer.
    pub multiplexer: MultiplexerKind,
    /// Keyboard protocol.
    pub keyboard: KeyboardProtocol,
    /// Cell pixel size.
    pub cell_px: (u16, u16),
    /// Max Ultra decks.
    pub max_decks: u8,
    /// Nitro (4000-char limit).
    pub nitro: bool,
    /// Initial theme name.
    pub theme: String,
    /// Prefer inline terminal video in the Stage deck over a pop-out window (§6.5; default `false`).
    pub prefer_inline_video: bool,
    /// Allow inline video on low tiers (Sixel/HalfBlock).
    pub inline_video_low_tier: bool,
    /// Allow inline video over SSH.
    pub inline_video_ssh: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            charset: ResolvedCharset::Utf8,
            depth: ColorDepth::TrueColor,
            emoji: EmojiPolicy::Strip,
            frame: FrameStyle::Double,
            gradient_borders: true,
            lcd_a02: false,
            ctrl_k: CtrlKMode::Dwim,
            wpm: 250,
            low_bandwidth: false,
            graphics: GraphicsTier::HalfBlock,
            over_ssh: false,
            multiplexer: MultiplexerKind::None,
            keyboard: KeyboardProtocol::LegacyVt,
            cell_px: (8, 16),
            max_decks: 3,
            nitro: false,
            theme: "ANSI-BBS Royal".to_string(),
            prefer_inline_video: false,
            inline_video_low_tier: false,
            inline_video_ssh: false,
        }
    }
}

impl TuiConfig {
    /// Derive from probed capabilities plus environment hints.
    ///
    /// `utf8_locale`: locale advertises UTF-8; `term`: value of `$TERM`.
    #[must_use]
    pub fn from_capabilities(
        caps: &TerminalCapabilities,
        utf8_locale: bool,
        term: Option<&str>,
    ) -> Self {
        let linux_vt = term == Some("linux");
        let dumb = term == Some("dumb");
        let charset = if dumb {
            ResolvedCharset::Ascii7Bit
        } else {
            caps.charset.resolve_with_vt(utf8_locale, linux_vt)
        };
        let emoji = if caps.grapheme_mode_2027 {
            EmojiPolicy::Mode2027
        } else {
            EmojiPolicy::Strip
        };
        let low_bandwidth = caps.over_ssh;
        let graphics = if charset == ResolvedCharset::Ascii7Bit
            || caps.color_depth == ColorDepth::Monochrome
        {
            GraphicsTier::TextTagOnly
        } else {
            caps.graphics_tier
        };
        let depth = if low_bandwidth && caps.color_depth == ColorDepth::TrueColor {
            ColorDepth::Ansi256
        } else {
            caps.color_depth
        };
        Self {
            charset,
            depth,
            emoji,
            gradient_borders: !low_bandwidth,
            low_bandwidth,
            graphics,
            over_ssh: caps.over_ssh,
            multiplexer: caps.multiplexer,
            keyboard: caps.keyboard,
            cell_px: caps.effective_cell_px(),
            ..Self::default()
        }
    }

    /// Text context for this config.
    #[must_use]
    pub fn text_ctx(&self) -> crate::text::TextCtx {
        crate::text::TextCtx {
            charset: self.charset,
            emoji: self.emoji,
            lcd_a02: self.lcd_a02,
        }
    }

    /// Message length limit.
    #[must_use]
    pub const fn max_message_len(&self) -> usize {
        if self.nitro { 4000 } else { 2000 }
    }

    /// Charset mode for display in settings.
    #[must_use]
    pub const fn charset_mode(&self) -> CharsetMode {
        match self.charset {
            ResolvedCharset::Utf8 => CharsetMode::Utf8,
            ResolvedCharset::VtSafe => CharsetMode::VtSafe,
            ResolvedCharset::Ascii7Bit => CharsetMode::Ascii7Bit,
        }
    }
}
