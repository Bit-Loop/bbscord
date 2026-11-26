//! `btop`-compatible semantic theme palette, 3-stop RGB gradients, ANSI-16 palettes, and color-depth contracts.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// Terminal color fidelity level for automatic palette quantization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum ColorDepth {
    /// 24-bit RGB SGR (`38;2;r;g;b` / `48;2;r;g;b`).
    #[default]
    TrueColor,
    /// 8-bit xterm-256 indexed color (`38;5;n`).
    Ansi256,
    /// Classic 16-color VGA / ANSI-BBS palette (`30..37`, `90..97`).
    Ansi16,
    /// Monochrome / 1-bit phosphor / HD44780 LCD (no color codes; reverse/bold only).
    Monochrome,
}

/// 24-bit sRGB color triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct RgbColor {
    /// Red channel (`0..=255`).
    pub r: u8,
    /// Green channel (`0..=255`).
    pub g: u8,
    /// Blue channel (`0..=255`).
    pub b: u8,
}

impl RgbColor {
    /// Construct a new RGB color.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Construct from a packed `0x00RRGGBB` integer (as used by Discord embed/role colors).
    #[must_use]
    pub const fn from_u32(rgb: u32) -> Self {
        Self {
            r: ((rgb >> 16) & 0xFF) as u8,
            g: ((rgb >> 8) & 0xFF) as u8,
            b: (rgb & 0xFF) as u8,
        }
    }

    /// Pack into `0x00RRGGBB`.
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    /// Format as lowercase `#rrggbb`.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    /// Parse a `btop` color literal:
    /// - `""` -> `Ok(None)` (transparent / omitted gradient stop)
    /// - `"#RRGGBB"` -> 24-bit RGB
    /// - `"#GG"` -> 2-hex grayscale `RGB(g, g, g)` (e.g. `adapta.theme`)
    /// - `"R G B"` -> space-separated decimal channels `0..=255`
    pub fn parse_btop(raw: &str) -> CoreResult<Option<Self>> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        if let Some(hex) = trimmed.strip_prefix('#') {
            match hex.len() {
                6 => {
                    let r = u8::from_str_radix(&hex[0..2], 16)
                        .map_err(|_| CoreError::ThemeParse(format!("invalid hex color '{trimmed}'")))?;
                    let g = u8::from_str_radix(&hex[2..4], 16)
                        .map_err(|_| CoreError::ThemeParse(format!("invalid hex color '{trimmed}'")))?;
                    let b = u8::from_str_radix(&hex[4..6], 16)
                        .map_err(|_| CoreError::ThemeParse(format!("invalid hex color '{trimmed}'")))?;
                    return Ok(Some(Self::new(r, g, b)));
                }
                2 => {
                    let gray = u8::from_str_radix(hex, 16)
                        .map_err(|_| CoreError::ThemeParse(format!("invalid grayscale hex '{trimmed}'")))?;
                    return Ok(Some(Self::new(gray, gray, gray)));
                }
                _ => {
                    return Err(CoreError::ThemeParse(format!(
                        "unsupported hex color length '{trimmed}'"
                    )));
                }
            }
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() == 3 {
            let r = parts[0]
                .parse::<u8>()
                .map_err(|_| CoreError::ThemeParse(format!("invalid red channel in '{trimmed}'")))?;
            let g = parts[1]
                .parse::<u8>()
                .map_err(|_| CoreError::ThemeParse(format!("invalid green channel in '{trimmed}'")))?;
            let b = parts[2]
                .parse::<u8>()
                .map_err(|_| CoreError::ThemeParse(format!("invalid blue channel in '{trimmed}'")))?;
            return Ok(Some(Self::new(r, g, b)));
        }

        Err(CoreError::ThemeParse(format!(
            "unrecognized btop color syntax '{trimmed}'"
        )))
    }

    /// Linearly interpolate between `self` and `other` with `t` clamped to `[0.0, 1.0]`.
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| -> u8 {
            let af = f32::from(a);
            let bf = f32::from(b);
            (af + (bf - af) * t).round().clamp(0.0, 255.0) as u8
        };
        Self {
            r: mix(self.r, other.r),
            g: mix(self.g, other.g),
            b: mix(self.b, other.b),
        }
    }

    /// Approximate sRGB relative luminance (`0.0..=1.0`) for contrast and monochrome fallbacks.
    #[must_use]
    pub fn relative_luminance(self) -> f32 {
        (0.2126 * f32::from(self.r) + 0.7152 * f32::from(self.g) + 0.0722 * f32::from(self.b))
            / 255.0
    }

    /// Quantize to the nearest xterm-256 color index (`16..=255`) using perceptual redmean distance
    /// over both the 6×6×6 color cube and the 24-step grayscale ramp (§1.4).
    #[must_use]
    pub fn to_ansi256(self) -> u8 {
        const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

        let mut best_idx = 16u8;
        let mut best_dist = u64::MAX;

        // Evaluate all 216 cube colors.
        for (ri, &rv) in CUBE_LEVELS.iter().enumerate() {
            for (gi, &gv) in CUBE_LEVELS.iter().enumerate() {
                for (bi, &bv) in CUBE_LEVELS.iter().enumerate() {
                    let candidate = Self::new(rv, gv, bv);
                    let d = self.redmean_dist_sq(candidate);
                    if d < best_dist {
                        best_dist = d;
                        best_idx = 16 + (ri as u8) * 36 + (gi as u8) * 6 + (bi as u8);
                    }
                }
            }
        }

        // Evaluate all 24 grayscale ramp entries (indices 232..=255 -> 8 + 10*i).
        for step in 0..24u8 {
            let v = 8 + step * 10;
            let candidate = Self::new(v, v, v);
            let d = self.redmean_dist_sq(candidate);
            if d < best_dist {
                best_dist = d;
                best_idx = 232 + step;
            }
        }

        best_idx
    }

    /// Quantize to the nearest standard ANSI 16-color index (`0..=15`) using redmean distance (§8.4).
    #[must_use]
    pub fn to_ansi16(self) -> u8 {
        const ANSI16: [RgbColor; 16] = [
            RgbColor::new(0, 0, 0),       // 0 Black
            RgbColor::new(170, 0, 0),     // 1 Red
            RgbColor::new(0, 170, 0),     // 2 Green
            RgbColor::new(170, 85, 0),    // 3 Yellow/Brown
            RgbColor::new(0, 0, 170),     // 4 Blue
            RgbColor::new(170, 0, 170),   // 5 Magenta
            RgbColor::new(0, 170, 170),   // 6 Cyan
            RgbColor::new(170, 170, 170), // 7 White
            RgbColor::new(85, 85, 85),    // 8 Bright Black (Dark Gray)
            RgbColor::new(255, 85, 85),   // 9 Bright Red
            RgbColor::new(85, 255, 85),   // 10 Bright Green
            RgbColor::new(255, 255, 85),  // 11 Bright Yellow
            RgbColor::new(85, 85, 255),   // 12 Bright Blue
            RgbColor::new(255, 85, 255),  // 13 Bright Magenta
            RgbColor::new(85, 255, 255),  // 14 Bright Cyan
            RgbColor::new(255, 255, 255), // 15 Bright White
        ];

        let mut best_idx = 0u8;
        let mut best_dist = u64::MAX;
        for (idx, candidate) in ANSI16.iter().enumerate() {
            let d = self.redmean_dist_sq(*candidate);
            if d < best_dist {
                best_dist = d;
                best_idx = idx as u8;
            }
        }
        best_idx
    }

    /// Integer-scaled perceptual redmean squared distance:
    /// `((512 + r_mean) * dr^2) + (1024 * dg^2) + ((767 - r_mean) * db^2)`.
    #[must_use]
    const fn redmean_dist_sq(self, other: Self) -> u64 {
        let r_mean = (self.r as u64 + other.r as u64) / 2;
        let dr = self.r.abs_diff(other.r) as u64;
        let dg = self.g.abs_diff(other.g) as u64;
        let db = self.b.abs_diff(other.b) as u64;
        (512 + r_mean) * dr * dr + 1024 * dg * dg + (767 - r_mean) * db * db
    }
}

impl fmt::Display for RgbColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// 1-, 2-, or 3-stop RGB gradient compatible with `btop` `*_start` / `*_mid` / `*_end` specs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColorGradient {
    /// Lower bound (`t = 0.0`) color.
    pub start: RgbColor,
    /// Optional midpoint (`t = 0.5`) color; when `None`, interpolates directly `start -> end`.
    pub mid: Option<RgbColor>,
    /// Optional upper bound (`t = 1.0`) color; when both `mid` and `end` are `None`, renders flat.
    pub end: Option<RgbColor>,
}

impl ColorGradient {
    /// Create a full 3-stop gradient (`start -> mid -> end`).
    #[must_use]
    pub const fn new_3stop(start: RgbColor, mid: RgbColor, end: RgbColor) -> Self {
        Self {
            start,
            mid: Some(mid),
            end: Some(end),
        }
    }

    /// Create a 2-stop gradient (`start -> end`).
    #[must_use]
    pub const fn new_2stop(start: RgbColor, end: RgbColor) -> Self {
        Self {
            start,
            mid: None,
            end: Some(end),
        }
    }

    /// Create a single-color flat gradient.
    #[must_use]
    pub const fn solid(color: RgbColor) -> Self {
        Self {
            start: color,
            mid: None,
            end: None,
        }
    }

    /// Sample the gradient at normalized position `t` in `[0.0, 1.0]`.
    #[must_use]
    pub fn sample(&self, t: f32) -> RgbColor {
        let t = t.clamp(0.0, 1.0);
        match (self.mid, self.end) {
            (Some(mid), Some(end)) => {
                if t <= 0.5 {
                    self.start.lerp(mid, t * 2.0)
                } else {
                    mid.lerp(end, (t - 0.5) * 2.0)
                }
            }
            (None, Some(end)) => self.start.lerp(end, t),
            (Some(mid), None) => self.start.lerp(mid, t),
            (None, None) => self.start,
        }
    }

    /// Convert each stop in this RGB gradient to its nearest ANSI-16 color index (`0..=15`).
    #[must_use]
    pub fn to_ansi16(self) -> Ansi16Gradient {
        Ansi16Gradient {
            start: self.start.to_ansi16(),
            mid: self.mid.map(RgbColor::to_ansi16),
            end: self.end.map(RgbColor::to_ansi16),
        }
    }
}

/// 1-, 2-, or 3-stop ANSI-16 indexed gradient (`0..=15`) matching [`ColorGradient`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ansi16Gradient {
    /// Lower-stop ANSI-16 index (`0..=15`).
    pub start: u8,
    /// Optional midpoint ANSI-16 index (`0..=15`).
    #[serde(default)]
    pub mid: Option<u8>,
    /// Optional upper-stop ANSI-16 index (`0..=15`).
    #[serde(default)]
    pub end: Option<u8>,
}

impl Ansi16Gradient {
    /// Create a 3-stop ANSI-16 gradient.
    #[must_use]
    pub const fn new_3stop(start: u8, mid: u8, end: u8) -> Self {
        Self {
            start: start & 0x0F,
            mid: Some(mid & 0x0F),
            end: Some(end & 0x0F),
        }
    }

    /// Create a 2-stop ANSI-16 gradient.
    #[must_use]
    pub const fn new_2stop(start: u8, end: u8) -> Self {
        Self {
            start: start & 0x0F,
            mid: None,
            end: Some(end & 0x0F),
        }
    }

    /// Create a flat 1-color ANSI-16 gradient.
    #[must_use]
    pub const fn solid(color: u8) -> Self {
        Self {
            start: color & 0x0F,
            mid: None,
            end: None,
        }
    }

    /// Sample the indexed gradient at normalized position `t` in `[0.0, 1.0]`.
    #[must_use]
    pub fn sample(&self, t: f32) -> u8 {
        let t = t.clamp(0.0, 1.0);
        let idx = match (self.mid, self.end) {
            (Some(mid), Some(end)) => {
                if t < 1.0 / 3.0 {
                    self.start
                } else if t < 2.0 / 3.0 {
                    mid
                } else {
                    end
                }
            }
            (None, Some(end)) => {
                if t < 0.5 {
                    self.start
                } else {
                    end
                }
            }
            (Some(mid), None) => {
                if t < 0.5 {
                    self.start
                } else {
                    mid
                }
            }
            (None, None) => self.start,
        };
        idx & 0x0F
    }
}

impl Default for Ansi16Gradient {
    fn default() -> Self {
        Self::new_3stop(4, 6, 15)
    }
}

/// Explicit ANSI-16 palette (`0..=15` SGR indices) mirroring every semantic key in [`ThemePalette`] (§8.3, §12.7).
///
/// Used both for `ColorDepth::Ansi16` themes and for the built-in `tty` theme which follows the
/// user's terminal palette at every color depth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ansi16Palette {
    /// Main background index (`None` = terminal default / SGR 49).
    pub main_bg: Option<u8>,
    /// Primary text color (`0..=15`).
    pub main_fg: u8,
    /// Box title color (`0..=15`).
    pub title: u8,
    /// Keyboard shortcut & highlight foreground (`0..=15`).
    pub hi_fg: u8,
    /// Selected list item background (`0..=15`).
    pub selected_bg: u8,
    /// Selected list item foreground (`0..=15`).
    pub selected_fg: u8,
    /// Muted / inactive text foreground (`0..=15`).
    pub inactive_fg: u8,
    /// Text drawn on top of graphs (`0..=15`).
    pub graph_text: u8,
    /// Meter track background color (`0..=15`).
    pub meter_bg: u8,
    /// Accent color for mentions and secondary telemetry (`0..=15`).
    pub proc_misc: u8,
    /// Sidebar / primary frame border (`0..=15`).
    pub cpu_box: u8,
    /// Chat / secondary frame border (`0..=15`).
    pub mem_box: u8,
    /// Voice / Stage / Gateway frame border (`0..=15`).
    pub net_box: u8,
    /// Members / decks frame border (`0..=15`).
    pub proc_box: u8,
    /// Internal divider line color (`0..=15`).
    pub div_line: u8,

    /// Latency / severity indexed gradient.
    pub temp_gradient: Ansi16Gradient,
    /// Border / primary VU indexed gradient.
    pub cpu_gradient: Ansi16Gradient,
    /// Free / fallback nick palette indexed gradient.
    pub free_gradient: Ansi16Gradient,
    /// Cached / fallback nick palette indexed gradient.
    pub cached_gradient: Ansi16Gradient,
    /// Available / fallback nick palette indexed gradient.
    pub available_gradient: Ansi16Gradient,
    /// Unread heat indexed gradient.
    pub used_gradient: Ansi16Gradient,
    /// Remote speaker VU indexed gradient.
    pub download_gradient: Ansi16Gradient,
    /// Local mic VU indexed gradient.
    pub upload_gradient: Ansi16Gradient,
    /// Selection list indexed gradient.
    pub process_gradient: Ansi16Gradient,
}

impl Ansi16Palette {
    /// Built-in `tty` 16-color indexed palette following the terminal's own SGR palette (§8.2).
    #[must_use]
    pub const fn tty_default() -> Self {
        Self {
            main_bg: Some(0),
            main_fg: 7,
            title: 15,
            hi_fg: 14,
            selected_bg: 4,
            selected_fg: 15,
            inactive_fg: 8,
            graph_text: 15,
            meter_bg: 8,
            proc_misc: 13,
            cpu_box: 4,
            mem_box: 6,
            net_box: 2,
            proc_box: 12,
            div_line: 8,
            temp_gradient: Ansi16Gradient::new_3stop(2, 11, 9),
            cpu_gradient: Ansi16Gradient::new_3stop(4, 6, 15),
            free_gradient: Ansi16Gradient::new_3stop(6, 12, 14),
            cached_gradient: Ansi16Gradient::new_3stop(2, 6, 14),
            available_gradient: Ansi16Gradient::new_3stop(3, 11, 9),
            used_gradient: Ansi16Gradient::new_3stop(2, 11, 9),
            download_gradient: Ansi16Gradient::new_3stop(2, 6, 14),
            upload_gradient: Ansi16Gradient::new_3stop(5, 13, 9),
            process_gradient: Ansi16Gradient::new_3stop(4, 6, 15),
        }
    }

    /// Auto-derive an [`Ansi16Palette`] from an RGB [`ThemePalette`] via redmean nearest-match (§8.4, §12.7).
    #[must_use]
    pub fn from_rgb_palette(p: &ThemePalette) -> Self {
        Self {
            main_bg: p.main_bg.map(RgbColor::to_ansi16),
            main_fg: p.main_fg.to_ansi16(),
            title: p.title.to_ansi16(),
            hi_fg: p.hi_fg.to_ansi16(),
            selected_bg: p.selected_bg.to_ansi16(),
            selected_fg: p.selected_fg.to_ansi16(),
            inactive_fg: p.inactive_fg.to_ansi16(),
            graph_text: p.graph_text.to_ansi16(),
            meter_bg: p.meter_bg.to_ansi16(),
            proc_misc: p.proc_misc.to_ansi16(),
            cpu_box: p.cpu_box.to_ansi16(),
            mem_box: p.mem_box.to_ansi16(),
            net_box: p.net_box.to_ansi16(),
            proc_box: p.proc_box.to_ansi16(),
            div_line: p.div_line.to_ansi16(),
            temp_gradient: p.temp_gradient.to_ansi16(),
            cpu_gradient: p.cpu_gradient.to_ansi16(),
            free_gradient: p.free_gradient.to_ansi16(),
            cached_gradient: p.cached_gradient.to_ansi16(),
            available_gradient: p.available_gradient.to_ansi16(),
            used_gradient: p.used_gradient.to_ansi16(),
            download_gradient: p.download_gradient.to_ansi16(),
            upload_gradient: p.upload_gradient.to_ansi16(),
            process_gradient: p.process_gradient.to_ansi16(),
        }
    }
}

impl Default for Ansi16Palette {
    fn default() -> Self {
        Self::tty_default()
    }
}

/// Complete `btop`-compatible semantic palette plus 3-stop meter/border gradients and optional ANSI-16 table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemePalette {
    /// Main background (`None` means terminal default / transparent).
    pub main_bg: Option<RgbColor>,
    /// Primary text color.
    pub main_fg: RgbColor,
    /// Box title color.
    pub title: RgbColor,
    /// Keyboard shortcut & highlight foreground.
    pub hi_fg: RgbColor,
    /// Selected list item background.
    pub selected_bg: RgbColor,
    /// Selected list item foreground.
    pub selected_fg: RgbColor,
    /// Muted / inactive text foreground.
    pub inactive_fg: RgbColor,
    /// Text drawn on top of Braille graphs / telemetry sparklines.
    pub graph_text: RgbColor,
    /// Meter track background color.
    pub meter_bg: RgbColor,
    /// Accent color for secondary badges, timestamps, and misc telemetry.
    pub proc_misc: RgbColor,
    /// Guilds / primary BBS outer frame border (`btop` `cpu_box`).
    pub cpu_box: RgbColor,
    /// Channels / DMs frame border (`btop` `mem_box`).
    pub mem_box: RgbColor,
    /// Voice / DAVE / Gateway frame border (`btop` `net_box`).
    pub net_box: RgbColor,
    /// Main chat / active stage frame border (`btop` `proc_box`).
    pub proc_box: RgbColor,
    /// Internal divider line color (`btop` `div_line`).
    pub div_line: RgbColor,

    /// Temperature / alert severity gradient.
    pub temp_gradient: ColorGradient,
    /// Primary activity / border / VU meter gradient (`cpu_*`).
    pub cpu_gradient: ColorGradient,
    /// Available headroom gradient (`free_*`).
    pub free_gradient: ColorGradient,
    /// Cached / buffer gradient (`cached_*`).
    pub cached_gradient: ColorGradient,
    /// Secondary capacity gradient (`available_*`).
    pub available_gradient: ColorGradient,
    /// Saturation / load gradient (`used_*`).
    pub used_gradient: ColorGradient,
    /// Incoming traffic / remote speakers VU gradient (`download_*`).
    pub download_gradient: ColorGradient,
    /// Outgoing traffic / mic VU gradient (`upload_*`).
    pub upload_gradient: ColorGradient,
    /// Active process / selection intensity gradient (`process_*`).
    pub process_gradient: ColorGradient,

    /// Optional explicit ANSI-16 indexed palette (`[ansi16]` in TOML themes, §12.7).
    /// When `None`, `bbs-tui` derives indices with [`RgbColor::to_ansi16`] via [`Self::resolved_ansi16`].
    #[serde(default)]
    pub ansi16: Option<Ansi16Palette>,
}

impl ThemePalette {
    /// Classic ANSI-BBS Royal baseline palette (cobalt frame, electric cyan accents, gold titles).
    #[must_use]
    pub const fn ansi_bbs_royal() -> Self {
        Self {
            main_bg: Some(RgbColor::new(10, 16, 36)),
            main_fg: RgbColor::new(226, 232, 240),
            title: RgbColor::new(250, 204, 21),
            hi_fg: RgbColor::new(56, 189, 248),
            selected_bg: RgbColor::new(30, 64, 175),
            selected_fg: RgbColor::new(255, 255, 255),
            inactive_fg: RgbColor::new(100, 116, 139),
            graph_text: RgbColor::new(241, 245, 249),
            meter_bg: RgbColor::new(30, 41, 59),
            proc_misc: RgbColor::new(168, 85, 247),
            cpu_box: RgbColor::new(59, 130, 246),
            mem_box: RgbColor::new(34, 211, 238),
            net_box: RgbColor::new(52, 211, 153),
            proc_box: RgbColor::new(96, 165, 250),
            div_line: RgbColor::new(51, 65, 85),
            temp_gradient: ColorGradient::new_3stop(
                RgbColor::new(56, 189, 248),
                RgbColor::new(250, 204, 21),
                RgbColor::new(239, 68, 68),
            ),
            cpu_gradient: ColorGradient::new_3stop(
                RgbColor::new(34, 197, 94),
                RgbColor::new(250, 204, 21),
                RgbColor::new(239, 68, 68),
            ),
            free_gradient: ColorGradient::new_3stop(
                RgbColor::new(56, 189, 248),
                RgbColor::new(59, 130, 246),
                RgbColor::new(99, 102, 241),
            ),
            cached_gradient: ColorGradient::new_3stop(
                RgbColor::new(45, 212, 191),
                RgbColor::new(34, 211, 238),
                RgbColor::new(56, 189, 248),
            ),
            available_gradient: ColorGradient::new_3stop(
                RgbColor::new(250, 204, 21),
                RgbColor::new(251, 146, 60),
                RgbColor::new(239, 68, 68),
            ),
            used_gradient: ColorGradient::new_3stop(
                RgbColor::new(34, 197, 94),
                RgbColor::new(234, 179, 8),
                RgbColor::new(239, 68, 68),
            ),
            download_gradient: ColorGradient::new_3stop(
                RgbColor::new(34, 211, 238),
                RgbColor::new(59, 130, 246),
                RgbColor::new(168, 85, 247),
            ),
            upload_gradient: ColorGradient::new_3stop(
                RgbColor::new(168, 85, 247),
                RgbColor::new(236, 72, 153),
                RgbColor::new(244, 63, 94),
            ),
            process_gradient: ColorGradient::new_3stop(
                RgbColor::new(52, 211, 153),
                RgbColor::new(56, 189, 248),
                RgbColor::new(168, 85, 247),
            ),
            ansi16: None,
        }
    }

    /// Return the explicit [`Ansi16Palette`] when provided, or derive one via redmean [`RgbColor::to_ansi16`].
    #[must_use]
    pub fn resolved_ansi16(&self) -> Ansi16Palette {
        self.ansi16
            .clone()
            .unwrap_or_else(|| Ansi16Palette::from_rgb_palette(self))
    }

    /// Semantic VU meter gradient (maps voice RMS `0.0..=1.0` cleanly onto `cpu_gradient`).
    #[must_use]
    pub const fn vu_meter_gradient(&self) -> ColorGradient {
        self.cpu_gradient
    }

    /// Semantic gateway latency sparkline gradient.
    #[must_use]
    pub const fn latency_gradient(&self) -> ColorGradient {
        self.temp_gradient
    }
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self::ansi_bbs_royal()
    }
}

/// Origin of a loaded theme specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThemeSource {
    /// Compiled-in theme preset.
    BuiltIn,
    /// Native `bbscord` TOML theme file.
    TomlFile,
    /// Imported directly from a `btop` `.theme` file.
    BtopThemeFile,
}

/// Named theme specification consumable by `bbs-tui` and theme switchers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeSpec {
    /// Human-readable theme name.
    pub name: String,
    /// Where this theme originated.
    pub source: ThemeSource,
    /// Semantic color and gradient palette.
    pub palette: ThemePalette,
}

impl Default for ThemeSpec {
    fn default() -> Self {
        Self {
            name: "ANSI-BBS Royal".to_string(),
            source: ThemeSource::BuiltIn,
            palette: ThemePalette::ansi_bbs_royal(),
        }
    }
}

/// Parse a real `btop` `.theme` file (`theme[key]="value"`) into a [`ThemeSpec`].
///
/// Handles real-world quirks observed across `/usr/share/btop/themes` (§8.4 & §12.6):
/// - Inline trailing comments after closing quote (e.g. `theme[cpu_mid]="#F8F8F2" #2eb398"`)
/// - 2-hex grayscale shorthands (`"#80"`)
/// - Space-separated RGB decimals (`"40 42 54"`)
/// - Empty `""` values (`main_bg=""` = transparent; `end == ""` => flat `start` with `mid` ignored
///   matching btop's `generateGradients`; `mid == ""` with `end` set => 2-stop `start -> end`)
/// - Optional/newer keys default gracefully (`graph_text -> main_fg`, `meter_bg -> inactive_fg`,
///   `process_* -> selected_bg` gradient); unknown keys are ignored.
pub fn parse_btop_theme(name: impl Into<String>, input: &str) -> CoreResult<ThemeSpec> {
    let mut raw_map: HashMap<String, String> = HashMap::new();

    for (line_idx, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("theme[") else {
            continue;
        };
        let Some((key, after_bracket)) = rest.split_once(']') else {
            return Err(CoreError::ThemeParse(format!(
                "line {}: missing closing ']' in theme entry",
                line_idx + 1
            )));
        };
        let Some((_, after_eq)) = after_bracket.split_once('=') else {
            return Err(CoreError::ThemeParse(format!(
                "line {}: missing '=' in theme[{key}]",
                line_idx + 1
            )));
        };
        let after_eq = after_eq.trim_start();
        let value = if let Some(after_open_quote) = after_eq.strip_prefix('"') {
            let Some((quoted, _trailing)) = after_open_quote.split_once('"') else {
                return Err(CoreError::ThemeParse(format!(
                    "line {}: unterminated quote in theme[{key}]",
                    line_idx + 1
                )));
            };
            quoted.trim().to_string()
        } else {
            after_eq
                .split('#')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        };
        raw_map.insert(key.trim().to_string(), value);
    }

    if raw_map.is_empty() {
        return Err(CoreError::ThemeParse(
            "no theme[...] entries found in btop theme file".to_string(),
        ));
    }

    let mut palette = ThemePalette::ansi_bbs_royal();

    let parse_opt = |key: &str| -> CoreResult<Option<Option<RgbColor>>> {
        match raw_map.get(key) {
            Some(v) => RgbColor::parse_btop(v).map(Some),
            None => Ok(None),
        }
    };

    if let Some(bg) = parse_opt("main_bg")? {
        palette.main_bg = bg;
    }
    if let Some(Some(c)) = parse_opt("main_fg")? {
        palette.main_fg = c;
    }
    if let Some(Some(c)) = parse_opt("title")? {
        palette.title = c;
    }
    if let Some(Some(c)) = parse_opt("hi_fg")? {
        palette.hi_fg = c;
    }
    if let Some(Some(c)) = parse_opt("selected_bg")? {
        palette.selected_bg = c;
    }
    if let Some(Some(c)) = parse_opt("selected_fg")? {
        palette.selected_fg = c;
    }
    if let Some(Some(c)) = parse_opt("inactive_fg")? {
        palette.inactive_fg = c;
    }
    palette.graph_text = match parse_opt("graph_text")? {
        Some(Some(c)) => c,
        _ => palette.main_fg,
    };
    palette.meter_bg = match parse_opt("meter_bg")? {
        Some(Some(c)) => c,
        _ => palette.inactive_fg,
    };
    if let Some(Some(c)) = parse_opt("proc_misc")? {
        palette.proc_misc = c;
    }
    if let Some(Some(c)) = parse_opt("cpu_box")? {
        palette.cpu_box = c;
    }
    if let Some(Some(c)) = parse_opt("mem_box")? {
        palette.mem_box = c;
    }
    if let Some(Some(c)) = parse_opt("net_box")? {
        palette.net_box = c;
    }
    if let Some(Some(c)) = parse_opt("proc_box")? {
        palette.proc_box = c;
    }
    if let Some(Some(c)) = parse_opt("div_line")? {
        palette.div_line = c;
    }

    let parse_gradient = |prefix: &str, fallback: ColorGradient| -> CoreResult<ColorGradient> {
        let start_key = format!("{prefix}_start");
        let mid_key = format!("{prefix}_mid");
        let end_key = format!("{prefix}_end");

        let start = match parse_opt(&start_key)? {
            Some(Some(c)) => c,
            _ => return Ok(fallback),
        };
        let end = parse_opt(&end_key)?.flatten();
        // §8.4 / §12.6: btop only interpolates when `*_end` is set; otherwise flat `start`.
        let mid = if end.is_some() {
            parse_opt(&mid_key)?.flatten()
        } else {
            None
        };
        Ok(ColorGradient { start, mid, end })
    };

    palette.temp_gradient = parse_gradient("temp", palette.temp_gradient)?;
    palette.cpu_gradient = parse_gradient("cpu", palette.cpu_gradient)?;
    palette.free_gradient = parse_gradient("free", palette.free_gradient)?;
    palette.cached_gradient = parse_gradient("cached", palette.cached_gradient)?;
    palette.available_gradient = parse_gradient("available", palette.available_gradient)?;
    palette.used_gradient = parse_gradient("used", palette.used_gradient)?;
    palette.download_gradient = parse_gradient("download", palette.download_gradient)?;
    palette.upload_gradient = parse_gradient("upload", palette.upload_gradient)?;
    palette.process_gradient =
        parse_gradient("process", ColorGradient::solid(palette.selected_bg))?;
    palette.ansi16 = None;

    Ok(ThemeSpec {
        name: name.into(),
        source: ThemeSource::BtopThemeFile,
        palette,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_btop_parsing_variants() {
        assert_eq!(RgbColor::parse_btop("").unwrap(), None);
        assert_eq!(
            RgbColor::parse_btop("#282a36").unwrap(),
            Some(RgbColor::new(40, 42, 54))
        );
        assert_eq!(
            RgbColor::parse_btop("#80").unwrap(),
            Some(RgbColor::new(128, 128, 128))
        );
        assert_eq!(
            RgbColor::parse_btop("12 34 56").unwrap(),
            Some(RgbColor::new(12, 34, 56))
        );
        assert!(RgbColor::parse_btop("#1234").is_err());
        assert!(RgbColor::parse_btop("999 0 0").is_err());
    }

    #[test]
    fn gradient_sampling_1_2_and_3_stops() {
        let g3 = ColorGradient::new_3stop(
            RgbColor::new(0, 0, 0),
            RgbColor::new(100, 100, 100),
            RgbColor::new(200, 200, 200),
        );
        assert_eq!(g3.sample(0.0), RgbColor::new(0, 0, 0));
        assert_eq!(g3.sample(0.5), RgbColor::new(100, 100, 100));
        assert_eq!(g3.sample(1.0), RgbColor::new(200, 200, 200));

        let g2 = ColorGradient::new_2stop(RgbColor::new(0, 0, 0), RgbColor::new(200, 100, 0));
        assert_eq!(g2.sample(0.5), RgbColor::new(100, 50, 0));

        let g1 = ColorGradient::solid(RgbColor::new(42, 42, 42));
        assert_eq!(g1.sample(0.75), RgbColor::new(42, 42, 42));
    }

    #[test]
    fn parse_real_btop_quirks_and_empty_end_rule() {
        let sample = r##"
# Comment line
theme[main_bg]=""
theme[main_fg]="#f8f8f2"
theme[title]="#ff"
theme[hi_fg]="98 114 164"
theme[selected_bg]="#ff79c6"   
theme[selected_fg]="#ffffff"
theme[inactive_fg]="#44475a"
theme[cpu_start]="#bd93f9"
theme[cpu_mid]="#F8F8F2" #2eb398"
theme[cpu_end]="#50fa7b"
theme[temp_start]="#62a0ea"
theme[temp_mid]=""
theme[temp_end]="#e01b24"
theme[free_start]="#22c55e"
theme[free_mid]=""
theme[free_end]=""
theme[used_start]="#112233"
theme[used_mid]="#445566"
theme[used_end]=""
theme[unknown_future_key]="#112233"
"##;

        let spec = parse_btop_theme("quirks", sample).unwrap();
        assert_eq!(spec.source, ThemeSource::BtopThemeFile);
        assert_eq!(spec.palette.main_bg, None);
        assert_eq!(spec.palette.title, RgbColor::new(255, 255, 255));
        assert_eq!(spec.palette.hi_fg, RgbColor::new(98, 114, 164));
        assert_eq!(
            spec.palette.cpu_gradient.mid,
            Some(RgbColor::new(248, 248, 242))
        );
        // 2-stop when mid="" and end is set
        assert_eq!(spec.palette.temp_gradient.mid, None);
        assert_eq!(
            spec.palette.temp_gradient.end,
            Some(RgbColor::new(224, 27, 36))
        );
        // 1-stop flat when both mid and end are ""
        assert_eq!(spec.palette.free_gradient.mid, None);
        assert_eq!(spec.palette.free_gradient.end, None);
        // §12.6: mid set with end="" MUST drop mid and stay flat `start` (btop-faithful)
        assert_eq!(
            spec.palette.used_gradient,
            ColorGradient::solid(RgbColor::new(0x11, 0x22, 0x33))
        );
        // Omitted optional keys default per §8.4
        assert_eq!(spec.palette.graph_text, spec.palette.main_fg);
        assert_eq!(spec.palette.meter_bg, spec.palette.inactive_fg);
        assert_eq!(
            spec.palette.process_gradient,
            ColorGradient::solid(spec.palette.selected_bg)
        );
        assert_eq!(spec.palette.ansi16, None);
    }

    #[test]
    fn ansi16_palette_explicit_and_derived() {
        let mut pal = ThemePalette::ansi_bbs_royal();
        let derived = pal.resolved_ansi16();
        assert_eq!(derived.selected_fg, 15);
        assert!(derived.main_bg.is_some());

        let tty = Ansi16Palette::tty_default();
        assert_eq!(tty.main_bg, Some(0));
        assert_eq!(tty.main_fg, 7);
        assert_eq!(tty.hi_fg, 14);
        assert_eq!(tty.cpu_gradient.sample(0.0), 4);
        assert_eq!(tty.cpu_gradient.sample(0.5), 6);
        assert_eq!(tty.cpu_gradient.sample(1.0), 15);

        pal.ansi16 = Some(tty.clone());
        assert_eq!(pal.resolved_ansi16(), tty);
    }

    #[test]
    fn color_quantization_sanity() {
        assert_eq!(RgbColor::new(0, 0, 0).to_ansi16(), 0);
        assert_eq!(RgbColor::new(255, 255, 255).to_ansi16(), 15);
        assert_eq!(RgbColor::new(255, 85, 85).to_ansi16(), 9);
        assert_eq!(RgbColor::new(0, 0, 0).to_ansi256(), 16);
        assert_eq!(RgbColor::new(255, 255, 255).to_ansi256(), 231);
    }

    #[test]
    fn parse_system_btop_themes_if_present() {
        let dir = std::path::Path::new("/usr/share/btop/themes");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut count = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("theme") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let spec = parse_btop_theme(name, &content)
                .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
            assert_eq!(spec.source, ThemeSource::BtopThemeFile);
            count += 1;
        }
        assert!(count > 0);
    }
}
