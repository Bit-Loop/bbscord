//! Command-line argument definitions and capability override conversions.

use std::path::PathBuf;

use bbs_core::{CharsetMode, ColorDepth, GraphicsTier, ViewportGeometry};
use bbs_runtime_term::parse_geometry_override;
use clap::{Parser, ValueEnum};

/// Retro BBS-styled Discord terminal client (`16×2` LCD through 4K ultrawide cockpits).
#[derive(Debug, Clone, Parser)]
#[command(
    name = "bbscord",
    version,
    about = "Retro BBS-styled Rust Discord TUI client with DAVE E2EE voice, Group DMs, Go Live screensharing, animated GIFs, and btop themes",
    long_about = "bbscord is a production-ready BBS-styled Discord terminal client scaling continuously\n\
                  from 16x2 HD44780 character LCDs to dual-2K/4K ultrawide cockpits with universal\n\
                  terminal graphics, DAVE MLS E2EE voice & Group DM calls, GStreamer WebRTC Go Live\n\
                  screensharing, animated GIF search, and live btop theme switching."
)]
pub struct Cli {
    /// Launch interactive TUI against deterministic 100% offline mock Discord/GIF/Voice/Video backends.
    #[arg(long)]
    pub mock_demo: bool,

    /// Run 10 full integrated render/update frames across 16x2, 80x24, and 512x144 against the mock stack + theme catalog and exit 0.
    #[arg(long)]
    pub headless_smoke: bool,

    /// Override terminal geometry (format: COLSxROWS, e.g. 16x2, 80x24, 512x144).
    #[arg(long, value_name = "COLSxROWS", value_parser = parse_geometry_arg)]
    pub geometry: Option<ViewportGeometry>,

    /// Override character-set rendering tier.
    #[arg(long, value_enum)]
    pub charset: Option<CharsetArg>,

    /// Enable HD44780 ROM A02 extended 7-bit ASCII characters (`\` and `~`).
    #[arg(long)]
    pub lcd_a02: bool,

    /// Override terminal graphics rendering tier.
    #[arg(long, value_enum)]
    pub graphics: Option<GraphicsArg>,

    /// Override terminal color depth.
    #[arg(long, value_enum)]
    pub color: Option<ColorArg>,

    /// Override active theme by built-in name, slug (e.g. `amber-phosphor`), user theme, or `btop/<name>`.
    #[arg(long, value_name = "NAME")]
    pub theme: Option<String>,

    /// Opt-in read-only import of Discord token from OS keyring (`service="discordo", user="token"`) purely in RAM (never written to disk).
    #[arg(long)]
    pub import_discordo_keyring: bool,

    /// Skip active terminal escape-sequence probes (`DA1`, Kitty, `DECRQM`, `XTWINOPS`).
    #[arg(long)]
    pub no_probe: bool,

    /// Enable explicit tmux DCS passthrough wrapping for Kitty/Sixel/iTerm2 graphics.
    #[arg(long)]
    pub tmux_passthrough: bool,

    /// Path to configuration TOML file (defaults to `~/.config/bbscord/config.toml`).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
}

/// CLI `--charset` argument variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CharsetArg {
    /// Auto-detect from locale and `$TERM`.
    Auto,
    /// Rich UTF-8 box drawing, Braille meters, and superscripts.
    Utf8,
    /// Linux virtual console PSF-safe glyphs (`TERM=linux`).
    #[value(name = "vtsafe")]
    VtSafe,
    /// Strict 7-bit ASCII safe for HD44780 LCDs and serial links.
    Ascii,
}

impl From<CharsetArg> for CharsetMode {
    fn from(value: CharsetArg) -> Self {
        match value {
            CharsetArg::Auto => Self::Auto,
            CharsetArg::Utf8 => Self::Utf8,
            CharsetArg::VtSafe => Self::VtSafe,
            CharsetArg::Ascii => Self::Ascii7Bit,
        }
    }
}

/// CLI `--graphics` argument variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GraphicsArg {
    /// Auto-negotiate highest safe tier from terminal probe.
    Auto,
    /// Kitty graphics protocol using Unicode placeholders (`U+10EEEE`).
    #[value(name = "kitty-placeholder")]
    KittyPlaceholder,
    /// Direct Kitty graphics protocol transmissions.
    #[value(name = "kitty-direct")]
    KittyDirect,
    /// DEC Sixel raster graphics.
    Sixel,
    /// iTerm2 / WezTerm `OSC 1337` inline images.
    #[value(name = "iterm2")]
    ITerm2,
    /// TrueColor/256-color Unicode half-blocks (`▀`).
    #[value(name = "halfblocks")]
    HalfBlocks,
    /// Text-only attachment/image tags (`[IMG name WxH]`).
    Tags,
}

impl GraphicsArg {
    /// Convert into an optional explicit [`GraphicsTier`] (`None` for `Auto`).
    #[must_use]
    pub const fn to_tier_override(self) -> Option<GraphicsTier> {
        match self {
            Self::Auto => None,
            Self::KittyPlaceholder => Some(GraphicsTier::KittyPlaceholder),
            Self::KittyDirect => Some(GraphicsTier::KittyDirect),
            Self::Sixel => Some(GraphicsTier::Sixel),
            Self::ITerm2 => Some(GraphicsTier::ITerm2),
            Self::HalfBlocks => Some(GraphicsTier::HalfBlock),
            Self::Tags => Some(GraphicsTier::TextTagOnly),
        }
    }
}

/// CLI `--color` argument variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorArg {
    /// Auto-detect color depth from environment.
    Auto,
    /// 24-bit TrueColor RGB.
    Truecolor,
    /// 8-bit xterm-256 palette.
    Ansi256,
    /// 16-color VGA / ANSI-BBS palette.
    Ansi16,
    /// Monochrome / 1-bit phosphor / LCD.
    Mono,
}

impl ColorArg {
    /// Convert into an optional explicit [`ColorDepth`] (`None` for `Auto`).
    #[must_use]
    pub const fn to_depth_override(self) -> Option<ColorDepth> {
        match self {
            Self::Auto => None,
            Self::Truecolor => Some(ColorDepth::TrueColor),
            Self::Ansi256 => Some(ColorDepth::Ansi256),
            Self::Ansi16 => Some(ColorDepth::Ansi16),
            Self::Mono => Some(ColorDepth::Monochrome),
        }
    }
}

fn parse_geometry_arg(raw: &str) -> Result<ViewportGeometry, String> {
    parse_geometry_override(raw).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_all_documented_flags() {
        let cli = Cli::try_parse_from([
            "bbscord",
            "--mock-demo",
            "--geometry",
            "16x2",
            "--charset",
            "ascii",
            "--lcd-a02",
            "--graphics",
            "tags",
            "--color",
            "mono",
            "--theme",
            "amber-phosphor",
            "--import-discordo-keyring",
            "--no-probe",
            "--tmux-passthrough",
            "--config",
            "/tmp/custom.toml",
        ])
        .unwrap();

        assert!(cli.mock_demo);
        assert!(!cli.headless_smoke);
        assert_eq!(cli.geometry, Some(ViewportGeometry::new(16, 2)));
        assert_eq!(cli.charset, Some(CharsetArg::Ascii));
        assert!(cli.lcd_a02);
        assert_eq!(cli.graphics, Some(GraphicsArg::Tags));
        assert_eq!(cli.color, Some(ColorArg::Mono));
        assert_eq!(cli.theme.as_deref(), Some("amber-phosphor"));
        assert!(cli.import_discordo_keyring);
        assert!(cli.no_probe);
        assert!(cli.tmux_passthrough);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/custom.toml")));
    }

    #[test]
    fn cli_parses_headless_smoke_and_vtsafe() {
        let cli = Cli::try_parse_from([
            "bbscord",
            "--headless-smoke",
            "--charset",
            "vtsafe",
            "--graphics",
            "kitty-placeholder",
        ])
        .unwrap();

        assert!(cli.headless_smoke);
        assert_eq!(
            cli.charset.map(CharsetMode::from),
            Some(CharsetMode::VtSafe)
        );
        assert_eq!(
            cli.graphics.and_then(GraphicsArg::to_tier_override),
            Some(GraphicsTier::KittyPlaceholder)
        );
    }

    #[test]
    fn cli_rejects_invalid_geometry() {
        assert!(Cli::try_parse_from(["bbscord", "--geometry", "0x24"]).is_err());
        assert!(Cli::try_parse_from(["bbscord", "--geometry", "wide"]).is_err());
    }
}
