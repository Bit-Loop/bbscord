//! Viewport geometry, layout breakpoints with hysteresis, graphics capability tiers, and scroll anchors.

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

    /// Resolve the raw responsive [`LayoutProfile`] for this viewport without hysteresis.
    #[must_use]
    pub const fn layout_profile(self) -> LayoutProfile {
        LayoutProfile::from_dimensions(self.cols, self.rows)
    }

    /// Resolve the responsive [`LayoutProfile`] applying §2.1 upgrade hysteresis against `prev`.
    #[must_use]
    pub const fn classify_profile(self, prev: Option<LayoutProfile>) -> LayoutProfile {
        LayoutProfile::classify(self, prev)
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
    /// Classify terminal cell dimensions into a raw [`LayoutProfile`] (no hysteresis).
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

    /// Classify a [`ViewportGeometry`] into a raw [`LayoutProfile`] (no hysteresis).
    #[must_use]
    pub const fn from_geometry(geometry: ViewportGeometry) -> Self {
        Self::from_dimensions(geometry.cols, geometry.rows)
    }

    /// Classify a [`ViewportGeometry`] with §2.1 asymmetric hysteresis against `prev`:
    /// - **Downgrade** (`raw < prev`): immediate so content always fits.
    /// - **Upgrade** (`raw > prev`): requires clearing every crossed threshold by `+2` cols / `+1` row:
    ///   - `Micro16x2 -> Compact`: `cols >= 26 && rows >= 6`
    ///   - `Compact -> StandardBbs`: `cols >= 62 && rows >= 13`
    ///   - `StandardBbs -> Wide`: `cols >= 142` (and `rows >= 13` if upgrading from `<= Compact`)
    ///   - `Wide -> UltraWideCockpit`: `cols >= 242` (and `rows >= 13` if upgrading from `<= Compact`)
    #[must_use]
    pub const fn classify(geom: ViewportGeometry, prev: Option<Self>) -> Self {
        let raw = Self::from_geometry(geom);
        let Some(prev) = prev else {
            return raw;
        };

        // Immediate downgrade (or unchanged).
        if (raw as u8) <= (prev as u8) {
            return raw;
        }

        let cols = geom.cols;
        let rows = geom.rows;

        match prev {
            Self::Micro16x2 => {
                if cols >= 242 && rows >= 13 {
                    Self::UltraWideCockpit
                } else if cols >= 142 && rows >= 13 {
                    Self::Wide
                } else if cols >= 62 && rows >= 13 {
                    Self::StandardBbs
                } else if cols >= 26 && rows >= 6 {
                    Self::Compact
                } else {
                    Self::Micro16x2
                }
            }
            Self::Compact => {
                if cols >= 242 && rows >= 13 {
                    Self::UltraWideCockpit
                } else if cols >= 142 && rows >= 13 {
                    Self::Wide
                } else if cols >= 62 && rows >= 13 {
                    Self::StandardBbs
                } else {
                    Self::Compact
                }
            }
            Self::StandardBbs => {
                if cols >= 242 {
                    Self::UltraWideCockpit
                } else if cols >= 142 {
                    Self::Wide
                } else {
                    Self::StandardBbs
                }
            }
            Self::Wide => {
                if cols >= 242 {
                    Self::UltraWideCockpit
                } else {
                    Self::Wide
                }
            }
            Self::UltraWideCockpit => Self::UltraWideCockpit,
        }
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
    /// Detect from locale (`LC_ALL`/`LANG`) and terminal environment (`TERM=linux`, `TERM=dumb`).
    #[default]
    Auto,
    /// Full UTF-8 rich glyph set (`╔═╗`, `╟─╢`, Braille `⠀..⣿`, eighths `▁..█`, superscripts).
    Utf8,
    /// Linux console / PSF-safe subset (`┌─┐`, `═║╔╗╚╝`, `▌█`, `░▒▓`, ASCII sigils; no Braille).
    VtSafe,
    /// Strict 7-bit ASCII (`+---+`, `|`, `#`, `*`) safe for HD44780 16×2 LCD ROMs and VT100 links.
    Ascii7Bit,
}

/// Concrete resolved character set after evaluating [`CharsetMode::Auto`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResolvedCharset {
    /// Full Unicode UTF-8 rich glyphs available (`Utf8Rich`).
    Utf8,
    /// Linux VT / PSF 256-glyph safe box-drawing and block glyphs (`TERM=linux`).
    VtSafe,
    /// 7-bit printable ASCII only (`0x20..=0x7D \ {0x5C}` for HD44780 A00).
    Ascii7Bit,
}

impl ResolvedCharset {
    /// Returns `true` when only 7-bit printable ASCII bytes may be emitted.
    #[must_use]
    pub const fn is_ascii_only(self) -> bool {
        matches!(self, Self::Ascii7Bit)
    }

    /// Returns `true` when rich Unicode glyphs (Braille, eighths, superscripts) are available.
    #[must_use]
    pub const fn is_utf8_rich(self) -> bool {
        matches!(self, Self::Utf8)
    }
}

impl CharsetMode {
    /// Resolve `Auto` into a concrete [`ResolvedCharset`] given UTF-8 locale availability.
    #[must_use]
    pub const fn resolve(self, utf8_supported: bool) -> ResolvedCharset {
        self.resolve_with_vt(utf8_supported, false)
    }

    /// Resolve `Auto` into a concrete [`ResolvedCharset`] accounting for Linux VT consoles (`TERM=linux`).
    #[must_use]
    pub const fn resolve_with_vt(self, utf8_supported: bool, linux_vt: bool) -> ResolvedCharset {
        match self {
            Self::Auto => {
                if !utf8_supported {
                    ResolvedCharset::Ascii7Bit
                } else if linux_vt {
                    ResolvedCharset::VtSafe
                } else {
                    ResolvedCharset::Utf8
                }
            }
            Self::Utf8 => ResolvedCharset::Utf8,
            Self::VtSafe => ResolvedCharset::VtSafe,
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
    /// DECRQM Mode 2027 Unicode grapheme-cluster width support (`CSI ? 2027 h`).
    #[serde(default)]
    pub grapheme_mode_2027: bool,
    /// Terminal supports native Kitty animation frames (`a=f`/`a=a`; true on Kitty, not assumed on Ghostty).
    #[serde(default)]
    pub kitty_animation: bool,
    /// `tmux` verified `allow-passthrough` + DCS-wrapped Kitty query response.
    #[serde(default)]
    pub tmux_passthrough: bool,
    /// Probed single-cell pixel size `(cell_width_px, cell_height_px)` from `CSI 16t` / `14t` / `TIOCGWINSZ`.
    #[serde(default)]
    pub cell_px: Option<(u16, u16)>,
    /// Escape-key disambiguation timeout in milliseconds (`50` local, `100` SSH, `0` Kitty keyboard).
    #[serde(default = "default_esc_timeout_ms_value")]
    pub esc_timeout_ms: u16,
}

const fn default_esc_timeout_ms_value() -> u16 {
    50
}

impl TerminalCapabilities {
    /// Default fallback cell pixel dimensions (`8×16`) when neither `TIOCGWINSZ` nor `CSI 16t/14t` respond.
    pub const FALLBACK_CELL_PX: (u16, u16) = (8, 16);

    /// Compute the recommended `ESC` disambiguation timeout in milliseconds (§9.1).
    #[must_use]
    pub const fn default_esc_timeout_ms(keyboard: KeyboardProtocol, over_ssh: bool) -> u16 {
        match keyboard {
            KeyboardProtocol::KittyCsiU => 0,
            KeyboardProtocol::LegacyVt | KeyboardProtocol::ModifyOtherKeys => {
                if over_ssh {
                    100
                } else {
                    50
                }
            }
        }
    }

    /// Resolve effective `(cell_width_px, cell_height_px)` per §1.2 precedence:
    /// `TIOCGWINSZ` (`geometry.cell_pixels()`) -> probed `cell_px` -> `(8, 16)`, clamped `>= 1`.
    #[must_use]
    pub const fn effective_cell_px(&self) -> (u16, u16) {
        let raw = match self.geometry.cell_pixels() {
            Some(px) => px,
            None => match self.cell_px {
                Some(px) => px,
                None => Self::FALLBACK_CELL_PX,
            },
        };
        let cw = if raw.0 == 0 { 1 } else { raw.0 };
        let ch = if raw.1 == 0 { 1 } else { raw.1 };
        (cw, ch)
    }
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
            grapheme_mode_2027: false,
            kitty_animation: false,
            tmux_passthrough: false,
            cell_px: None,
            esc_timeout_ms: default_esc_timeout_ms_value(),
        }
    }
}

/// Structural position inside a rendered [`crate::model::Message`] used by [`ScrollAnchor`] (§5).
///
/// Anchoring by semantic part (rather than raw visual row offset) keeps reading position exact
/// across `640×180 -> 16×2 -> 640×180` resizes even when tall embeds, attachments, or reactions
/// are on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AnchorPart {
    /// Message header row (author + timestamp / reply reference).
    Header,
    /// Message text body at the grapheme-cluster index of the line's first cluster.
    Content {
        /// Zero-based grapheme cluster index within `message.content`.
        grapheme: u32,
    },
    /// Flattened text inside `message.embeds[index]` at the given grapheme cluster offset.
    Embed {
        /// Zero-based index into `message.embeds`.
        index: u16,
        /// Zero-based grapheme cluster index within the embed's flattened text.
        grapheme: u32,
    },
    /// Vertical position inside `message.attachments[index]` image/media box (`0..=1000` permille).
    Attachment {
        /// Zero-based index into `message.attachments`.
        index: u16,
        /// Fractional vertical position in thousandths (`0` = top row, `1000` = bottom row).
        permille: u16,
    },
    /// Message reactions bar.
    Reactions,
    /// Message footer / trailing metadata row.
    Footer,
}

impl Default for AnchorPart {
    fn default() -> Self {
        Self::Content { grapheme: 0 }
    }
}

/// Content-addressed scroll anchor that survives arbitrary `SIGWINCH` reflows without drift (§5).
///
/// The anchor identifies the **bottom-visible line** of the message timeline (chat grows upward).
/// Resizing never mutates the anchor; only explicit user scroll, `MESSAGE_DELETE`, or
/// `MESSAGE_UPDATE` clamping updates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScrollAnchor {
    /// Anchored message identifier.
    pub message_id: Snowflake,
    /// Semantic location inside the anchored message.
    #[serde(default)]
    pub part: AnchorPart,
}

impl ScrollAnchor {
    /// Construct a new anchor targeting `message_id` and a specific [`AnchorPart`].
    #[must_use]
    pub const fn new(message_id: Snowflake, part: AnchorPart) -> Self {
        Self { message_id, part }
    }

    /// Anchor at the message header.
    #[must_use]
    pub const fn at_header(message_id: Snowflake) -> Self {
        Self::new(message_id, AnchorPart::Header)
    }

    /// Anchor at a specific grapheme cluster inside `message.content`.
    #[must_use]
    pub const fn at_content(message_id: Snowflake, grapheme: u32) -> Self {
        Self::new(message_id, AnchorPart::Content { grapheme })
    }

    /// Anchor inside `message.embeds[index]` at `grapheme`.
    #[must_use]
    pub const fn at_embed(message_id: Snowflake, index: u16, grapheme: u32) -> Self {
        Self::new(message_id, AnchorPart::Embed { index, grapheme })
    }

    /// Anchor inside `message.attachments[index]` at `permille` (clamped to `0..=1000`).
    #[must_use]
    pub const fn at_attachment(message_id: Snowflake, index: u16, permille: u16) -> Self {
        let clamped = if permille > 1000 { 1000 } else { permille };
        Self::new(
            message_id,
            AnchorPart::Attachment {
                index,
                permille: clamped,
            },
        )
    }

    /// Anchor at the message's reactions bar.
    #[must_use]
    pub const fn at_reactions(message_id: Snowflake) -> Self {
        Self::new(message_id, AnchorPart::Reactions)
    }

    /// Anchor at the message's footer row.
    #[must_use]
    pub const fn at_footer(message_id: Snowflake) -> Self {
        Self::new(message_id, AnchorPart::Footer)
    }
}

/// High-level scroll tracking state for the active message timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ScrollState {
    /// Automatically track newest incoming messages at the bottom of the viewport.
    #[default]
    FollowTail,
    /// Pinned to a specific message and semantic [`AnchorPart`] across resizes.
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
    fn layout_profile_hysteresis_matches_spec_2_1() {
        // Without prev, classify matches raw.
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(24, 5), None),
            LayoutProfile::Compact
        );

        // Micro -> Compact requires cols >= 26 && rows >= 6
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(24, 5), Some(LayoutProfile::Micro16x2)),
            LayoutProfile::Micro16x2
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(25, 6), Some(LayoutProfile::Micro16x2)),
            LayoutProfile::Micro16x2
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(30, 5), Some(LayoutProfile::Micro16x2)),
            LayoutProfile::Micro16x2
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(26, 6), Some(LayoutProfile::Micro16x2)),
            LayoutProfile::Compact
        );

        // Compact -> Standard requires cols >= 62 && rows >= 13
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(60, 12), Some(LayoutProfile::Compact)),
            LayoutProfile::Compact
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(62, 12), Some(LayoutProfile::Compact)),
            LayoutProfile::Compact
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(61, 13), Some(LayoutProfile::Compact)),
            LayoutProfile::Compact
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(62, 13), Some(LayoutProfile::Compact)),
            LayoutProfile::StandardBbs
        );

        // Standard -> Wide requires cols >= 142
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(141, 24), Some(LayoutProfile::StandardBbs)),
            LayoutProfile::StandardBbs
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(142, 12), Some(LayoutProfile::StandardBbs)),
            LayoutProfile::Wide
        );

        // Wide -> Ultra requires cols >= 242
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(241, 40), Some(LayoutProfile::Wide)),
            LayoutProfile::Wide
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(242, 40), Some(LayoutProfile::Wide)),
            LayoutProfile::UltraWideCockpit
        );

        // Downgrades are always immediate
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(239, 40), Some(LayoutProfile::UltraWideCockpit)),
            LayoutProfile::Wide
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(139, 40), Some(LayoutProfile::Wide)),
            LayoutProfile::StandardBbs
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(80, 11), Some(LayoutProfile::StandardBbs)),
            LayoutProfile::Compact
        );
        assert_eq!(
            LayoutProfile::classify(ViewportGeometry::new(23, 40), Some(LayoutProfile::Compact)),
            LayoutProfile::Micro16x2
        );
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
    fn charset_resolution_including_vt_safe() {
        assert_eq!(CharsetMode::Auto.resolve(true), ResolvedCharset::Utf8);
        assert_eq!(CharsetMode::Auto.resolve(false), ResolvedCharset::Ascii7Bit);
        assert_eq!(
            CharsetMode::Auto.resolve_with_vt(true, true),
            ResolvedCharset::VtSafe
        );
        assert_eq!(
            CharsetMode::Auto.resolve_with_vt(false, true),
            ResolvedCharset::Ascii7Bit
        );
        assert_eq!(CharsetMode::VtSafe.resolve(true), ResolvedCharset::VtSafe);
        assert_eq!(CharsetMode::Ascii7Bit.resolve(true), ResolvedCharset::Ascii7Bit);
        assert_eq!(CharsetMode::Utf8.resolve(false), ResolvedCharset::Utf8);
    }

    #[test]
    fn terminal_capabilities_cell_px_and_esc_timeout() {
        let mut caps = TerminalCapabilities::default();
        assert_eq!(caps.esc_timeout_ms, 50);
        assert_eq!(caps.effective_cell_px(), (8, 16));

        caps.cell_px = Some((9, 18));
        assert_eq!(caps.effective_cell_px(), (9, 18));

        // TIOCGWINSZ wins over CSI 16t cell_px.
        caps.geometry = ViewportGeometry::with_pixels(100, 50, 1000, 1000);
        assert_eq!(caps.effective_cell_px(), (10, 20));

        assert_eq!(
            TerminalCapabilities::default_esc_timeout_ms(KeyboardProtocol::LegacyVt, false),
            50
        );
        assert_eq!(
            TerminalCapabilities::default_esc_timeout_ms(KeyboardProtocol::LegacyVt, true),
            100
        );
        assert_eq!(
            TerminalCapabilities::default_esc_timeout_ms(KeyboardProtocol::KittyCsiU, true),
            0
        );
    }

    #[test]
    fn scroll_anchor_parts_and_clamping() {
        let sf = Snowflake(42);
        let a = ScrollAnchor::at_attachment(sf, 1, 1500);
        assert_eq!(
            a,
            ScrollAnchor {
                message_id: sf,
                part: AnchorPart::Attachment {
                    index: 1,
                    permille: 1000
                }
            }
        );
        let e = ScrollAnchor::at_embed(sf, 0, 12);
        assert_eq!(e.part, AnchorPart::Embed { index: 0, grapheme: 12 });
    }
}
