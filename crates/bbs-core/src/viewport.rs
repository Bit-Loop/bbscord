//! Viewport geometry, layout breakpoints, graphics capability tiers, and scroll anchors.

use serde::{Deserialize, Serialize};

use crate::model::Snowflake;
use crate::theme::ColorDepth;

/// Terminal viewport dimensions in character cells and optional pixel dimensions.
///
/// `pixel_width` and `pixel_height` are `0` when the terminal or PTY does not report
/// `TIOCGWINSZ` pixel geometry (for example over serial consoles, `16x2` LCDs, or
/// older multiplexers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ViewportGeometry {
    /// Width of the terminal in character cells.
    pub cols: u16,
    /// Height of the terminal in character cells.
    pub rows: u16,
    /// Total viewport width in pixels (`0` if unavailable).
    pub pixel_width: u16,
    /// Total viewport height in pixels (`0` if unavailable).
    pub pixel_height: u16,
}

impl ViewportGeometry {
    /// Create a geometry with character dimensions and unknown pixel dimensions.
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// Create a geometry with explicit character and pixel dimensions.
    #[must_use]
    pub const fn with_pixels(cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> Self {
        Self {
            cols,
            rows,
            pixel_width,
            pixel_height,
        }
    }

    /// Resolve the active responsive [`LayoutProfile`] for this viewport.
    #[must_use]
    pub const fn layout_profile(self) -> LayoutProfile {
        LayoutProfile::from_dimensions(self.cols, self.rows)
    }

    /// Returns `true` when non-zero pixel dimensions are available for both axes.
    #[must_use]
    pub const fn has_pixel_metrics(self) -> bool {
        self.cols > 0 && self.rows > 0 && self.pixel_width > 0 && self.pixel_height > 0
    }

    /// Estimated `(cell_width_px, cell_height_px)` when pixel metrics are known.
    #[must_use]
    pub const fn cell_pixels(self) -> Option<(u16, u16)> {
        if !self.has_pixel_metrics() {
            return None;
        }
        let cw = self.pixel_width / self.cols;
        let ch = self.pixel_height / self.rows;
        if cw == 0 || ch == 0 {
            None
        } else {
            Some((cw, ch))
        }
    }

    /// Aspect ratio (`cell_width / cell_height`), falling back to `0.5` (typical 1:2 font cell).
    #[must_use]
    pub fn cell_aspect_ratio(self) -> f32 {
        match self.cell_pixels() {
            Some((cw, ch)) => f32::from(cw) / f32::from(ch),
            None => 0.5,
        }
    }

    /// Maximum number of side-by-side `inline` embed field columns recommended at this size.
    #[must_use]
    pub const fn embed_max_columns(self) -> usize {
        self.layout_profile().max_inline_embed_cols()
    }
}

impl Default for ViewportGeometry {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

/// Responsive layout profile spanning `16×2` character LCDs through `640×180` 4K ultrawide cockpits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LayoutProfile {
    /// `rows <= 4 || cols < 24` — hardware `16×2` / `20×4` LCDs and ultra-tiny panes.
    Micro16x2,
    /// `cols < 60 || rows < 12` (when not `Micro16x2`) — mobile SSH and narrow splits.
    Compact,
    /// `60..=139` cols and `>= 12` rows — classic `80×24` / `120×40` BBS layout.
    StandardBbs,
    /// `140..=239` cols and `>= 12` rows — 3-pane desktop with multi-column inline embeds.
    Wide,
    /// `>= 240` cols and `>= 12` rows — dual-2K / 4K multi-deck cockpit with clamped measure.
    UltraWideCockpit,
}

impl LayoutProfile {
    /// Classify terminal cell dimensions into a [`LayoutProfile`].
    #[must_use]
    pub const fn from_dimensions(cols: u16, rows: u16) -> Self {
        if rows <= 4 || cols < 24 {
            Self::Micro16x2
        } else if cols < 60 || rows < 12 {
            Self::Compact
        } else if cols < 140 {
            Self::StandardBbs
        } else if cols < 240 {
            Self::Wide
        } else {
            Self::UltraWideCockpit
        }
    }

    /// Classify a [`ViewportGeometry`] into a [`LayoutProfile`].
    #[must_use]
    pub const fn from_geometry(geometry: ViewportGeometry) -> Self {
        Self::from_dimensions(geometry.cols, geometry.rows)
    }

    /// Returns `true` when multiple simultaneous panes can be shown without overlay swapping.
    #[must_use]
    pub const fn supports_multi_pane(self) -> bool {
        matches!(self, Self::StandardBbs | Self::Wide | Self::UltraWideCockpit)
    }

    /// Maximum number of side-by-side inline embed columns appropriate for this profile.
    #[must_use]
    pub const fn max_inline_embed_cols(self) -> usize {
        match self {
            Self::Micro16x2 | Self::Compact => 1,
            Self::StandardBbs => 2,
            Self::Wide | Self::UltraWideCockpit => 3,
        }
    }
}

/// Character-set rendering preference for borders, meters, and sigils.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum CharsetMode {
    /// Detect from locale (`LC_ALL`/`LANG`) and terminal environment.
    #[default]
    Auto,
    /// Full UTF-8 box drawing (`╔═╗`, `╟─╢`), Braille VU meters (`⠀..⣿`), and Unicode glyphs.
    Utf8,
    /// Strict 7-bit ASCII (`+---+`, `|`, `#`, `*`) safe for HD44780 16×2 LCD ROMs and VT100 links.
    Ascii7Bit,
}

/// Concrete resolved character set after evaluating [`CharsetMode::Auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResolvedCharset {
    /// Unicode UTF-8 glyphs available.
    Utf8,
    /// 7-bit printable ASCII only.
    Ascii7Bit,
}

impl CharsetMode {
    /// Resolve `Auto` into a concrete [`ResolvedCharset`].
    #[must_use]
    pub const fn resolve(self, utf8_supported: bool) -> ResolvedCharset {
        match self {
            Self::Auto => {
                if utf8_supported {
                    ResolvedCharset::Utf8
                } else {
                    ResolvedCharset::Ascii7Bit
                }
            }
            Self::Utf8 => ResolvedCharset::Utf8,
            Self::Ascii7Bit => ResolvedCharset::Ascii7Bit,
        }
    }
}

/// Graphics rendering protocol tier ordered from highest fidelity to universal text fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GraphicsTier {
    /// Kitty graphics protocol using Unicode placeholders (`U+10EEEE`), safest inside modern panes.
    KittyPlaceholder,
    /// Direct Kitty graphics protocol transmissions.
    KittyDirect,
    /// DEC Sixel raster graphics (`DCS q ... ST`).
    Sixel,
    /// iTerm2 / WezTerm inline images (`OSC 1337`).
    ITerm2,
    /// TrueColor/256-color Unicode half-block (`▀`/`▄`) rasterization.
    HalfBlock,
    /// Pure text attachment/image placeholders (`[IMG 1920x1080]`), required on ASCII/micro displays.
    TextTagOnly,
}

impl GraphicsTier {
    /// Returns `true` for pixel-level terminal graphics protocols.
    #[must_use]
    pub const fn supports_pixel_images(self) -> bool {
        matches!(
            self,
            Self::KittyPlaceholder | Self::KittyDirect | Self::Sixel | Self::ITerm2
        )
    }

    /// Returns `true` if multi-frame animated previews are practical at this tier.
    #[must_use]
    pub const fn supports_animation(self) -> bool {
        !matches!(self, Self::TextTagOnly)
    }

    /// Return the next safer fallback tier when a protocol fails or is disabled.
    #[must_use]
    pub const fn fallback_next(self) -> Self {
        match self {
            Self::KittyPlaceholder => Self::KittyDirect,
            Self::KittyDirect => Self::Sixel,
            Self::Sixel => Self::ITerm2,
            Self::ITerm2 => Self::HalfBlock,
            Self::HalfBlock | Self::TextTagOnly => Self::TextTagOnly,
        }
    }
}

/// Detected terminal multiplexer wrapping the PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum MultiplexerKind {
    /// Direct terminal emulator without a detected multiplexer.
    #[default]
    None,
    /// `tmux` (`$TMUX`).
    Tmux,
    /// `zellij` (`$ZELLIJ`).
    Zellij,
    /// GNU `screen` (`$STY`).
    Screen,
}

/// Keyboard input enhancement level reported or inferred for the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum KeyboardProtocol {
    /// Classic VT/xterm escape sequences (universal baseline; never requires `CSI u`).
    #[default]
    LegacyVt,
    /// xterm `modifyOtherKeys` extension.
    ModifyOtherKeys,
    /// Kitty keyboard protocol (`CSI u`).
    KittyCsiU,
}

/// Full probed runtime capabilities of the attached terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCapabilities {
    /// Current viewport dimensions in cells and optional pixels.
    pub geometry: ViewportGeometry,
    /// Selected graphics rendering tier.
    pub graphics_tier: GraphicsTier,
    /// Selected color fidelity tier.
    pub color_depth: ColorDepth,
    /// Character set mode.
    pub charset: CharsetMode,
    /// Surrounding terminal multiplexer, if any.
    pub multiplexer: MultiplexerKind,
    /// Active keyboard protocol level.
    pub keyboard: KeyboardProtocol,
    /// DEC synchronized update mode (`CSI ? 2026 h/l`) support.
    pub synchronized_output: bool,
    /// Bracketed paste mode (`CSI ? 2004 h/l`) support.
    pub bracketed_paste: bool,
    /// SGR mouse reporting (`CSI ? 1006 h/l`) support.
    pub mouse_sgr: bool,
    /// OSC 52 clipboard write support.
    pub osc52_clipboard: bool,
    /// Whether the session is running over SSH (`SSH_CONNECTION`/`SSH_TTY`).
    pub over_ssh: bool,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            geometry: ViewportGeometry::default(),
            graphics_tier: GraphicsTier::HalfBlock,
            color_depth: ColorDepth::TrueColor,
            charset: CharsetMode::Auto,
            multiplexer: MultiplexerKind::None,
            keyboard: KeyboardProtocol::LegacyVt,
            synchronized_output: false,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52_clipboard: false,
            over_ssh: false,
        }
    }
}

/// Content-addressed scroll anchor that survives arbitrary `SIGWINCH` reflows without drift.
///
/// Instead of storing a fragile raw row index from the top of the message list, the viewport
/// pins a concrete Discord [`Snowflake`] plus the wrapped visual line offset inside that message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScrollAnchor {
    /// Anchored message identifier.
    pub message_id: Snowflake,
    /// Zero-based wrapped visual line offset within the anchored message.
    pub wrapped_line_offset: u32,
}

impl ScrollAnchor {
    /// Construct a new anchor at the start of a message or a specific wrapped line offset.
    #[must_use]
    pub const fn new(message_id: Snowflake, wrapped_line_offset: u32) -> Self {
        Self {
            message_id,
            wrapped_line_offset,
        }
    }
}

/// High-level scroll tracking state for the active message timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ScrollState {
    /// Automatically track newest incoming messages at the bottom of the viewport.
    #[default]
    FollowTail,
    /// Pinned to a specific message and wrapped line offset across resizes.
    Anchored(ScrollAnchor),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_profile_breakpoints_match_spec() {
        // Micro16x2: rows <= 4 || cols < 24
        assert_eq!(LayoutProfile::from_dimensions(16, 2), LayoutProfile::Micro16x2);
        assert_eq!(LayoutProfile::from_dimensions(20, 4), LayoutProfile::Micro16x2);
        assert_eq!(LayoutProfile::from_dimensions(23, 40), LayoutProfile::Micro16x2);
        assert_eq!(LayoutProfile::from_dimensions(120, 4), LayoutProfile::Micro16x2);

        // Compact: cols < 60 || rows < 12 (when not micro)
        assert_eq!(LayoutProfile::from_dimensions(24, 5), LayoutProfile::Compact);
        assert_eq!(LayoutProfile::from_dimensions(59, 24), LayoutProfile::Compact);
        assert_eq!(LayoutProfile::from_dimensions(100, 11), LayoutProfile::Compact);

        // StandardBbs: cols < 140 and rows >= 12
        assert_eq!(LayoutProfile::from_dimensions(60, 12), LayoutProfile::StandardBbs);
        assert_eq!(LayoutProfile::from_dimensions(80, 24), LayoutProfile::StandardBbs);
        assert_eq!(LayoutProfile::from_dimensions(139, 40), LayoutProfile::StandardBbs);

        // Wide: 140..=239 cols and rows >= 12
        assert_eq!(LayoutProfile::from_dimensions(140, 40), LayoutProfile::Wide);
        assert_eq!(LayoutProfile::from_dimensions(239, 60), LayoutProfile::Wide);

        // UltraWideCockpit: >= 240 cols and rows >= 12
        assert_eq!(LayoutProfile::from_dimensions(240, 60), LayoutProfile::UltraWideCockpit);
        assert_eq!(LayoutProfile::from_dimensions(512, 144), LayoutProfile::UltraWideCockpit);
        assert_eq!(LayoutProfile::from_dimensions(640, 180), LayoutProfile::UltraWideCockpit);
    }

    #[test]
    fn viewport_pixel_metrics_and_aspect_ratio() {
        let no_px = ViewportGeometry::new(80, 24);
        assert!(!no_px.has_pixel_metrics());
        assert_eq!(no_px.cell_pixels(), None);
        assert!((no_px.cell_aspect_ratio() - 0.5).abs() < f32::EPSILON);

        let with_px = ViewportGeometry::with_pixels(160, 50, 1600, 1000);
        assert!(with_px.has_pixel_metrics());
        assert_eq!(with_px.cell_pixels(), Some((10, 20)));
        assert!((with_px.cell_aspect_ratio() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn graphics_tier_fallback_chain_terminates() {
        let mut tier = GraphicsTier::KittyPlaceholder;
        let expected = [
            GraphicsTier::KittyDirect,
            GraphicsTier::Sixel,
            GraphicsTier::ITerm2,
            GraphicsTier::HalfBlock,
            GraphicsTier::TextTagOnly,
            GraphicsTier::TextTagOnly,
        ];
        for next in expected {
            tier = tier.fallback_next();
            assert_eq!(tier, next);
        }
    }

    #[test]
    fn charset_resolution() {
        assert_eq!(CharsetMode::Auto.resolve(true), ResolvedCharset::Utf8);
        assert_eq!(CharsetMode::Auto.resolve(false), ResolvedCharset::Ascii7Bit);
        assert_eq!(CharsetMode::Ascii7Bit.resolve(true), ResolvedCharset::Ascii7Bit);
        assert_eq!(CharsetMode::Utf8.resolve(false), ResolvedCharset::Utf8);
    }
}
