//! Theme engine: built-ins, discovery/loading, depth-aware color resolution, gradients (§8).
//!
//! Layering: [`bbs_core::ThemeSpec`] is pure data (RGB palette + optional `[ansi16]` table).
//! [`Theme`] binds a spec to a [`ColorDepth`] and precomputes every role colour and 16-step
//! gradient LUT for that depth, so per-cell lookups during rendering are array reads.
//!
//! Depth resolution:
//! - `TrueColor` → `Color::Rgb`
//! - `Ansi256` → `Color::Indexed(16..=255)` via core's redmean `to_ansi256` (cube + grey ramp)
//! - `Ansi16` → the theme's explicit `[ansi16]` table (or a redmean-derived one for imports)
//! - `Monochrome` → `Color::Reset`, with focus carried by modifiers in [`Theme::style`]
//!
//! The built-in `tty` theme always emits `Color::Indexed(0..=15)` so it follows the user's
//! terminal palette regardless of depth (except `Monochrome`, which never emits colour).

mod builtin;
mod loader;
pub mod oklab;

pub use builtin::{builtin_by_name, builtin_specs, is_tty_spec, slug, vga_rgb};
pub use loader::{
    CatalogEntry, MAX_THEME_FILE_BYTES, ThemeCatalog, ThemeDirs, ThemeError, ThemeOrigin,
    is_btop_format, parse_theme_file, to_theme_toml,
};
pub use oklab::GRADIENT_STEPS;

use bbs_core::{Ansi16Gradient, Ansi16Palette, ColorDepth, ColorGradient, RgbColor, ThemeSpec};
use ratatui::style::{Color, Modifier, Style};

/// Semantic color roles (§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Base background.
    Bg,
    /// Base text.
    Fg,
    /// Pane titles.
    Title,
    /// BBS hotkey letters / unread markers.
    Hotkey,
    /// Selection (fg = selected_fg, bg = selected_bg in `style`).
    Selected,
    /// Timestamps, read channels, dim lines.
    Inactive,
    /// Sparkline labels.
    GraphText,
    /// Empty meter cells.
    MeterBg,
    /// Mentions.
    Mention,
    /// Inner dividers.
    Divider,
    /// Sidebar border.
    BoxSidebar,
    /// Chat border.
    BoxChat,
    /// Voice / stage border.
    BoxVoice,
    /// Members / decks border.
    BoxMembers,
}

impl Role {
    /// Every role, in declaration order.
    pub const ALL: [Self; 14] = [
        Self::Bg,
        Self::Fg,
        Self::Title,
        Self::Hotkey,
        Self::Selected,
        Self::Inactive,
        Self::GraphText,
        Self::MeterBg,
        Self::Mention,
        Self::Divider,
        Self::BoxSidebar,
        Self::BoxChat,
        Self::BoxVoice,
        Self::BoxMembers,
    ];

    const fn idx(self) -> usize {
        self as usize
    }
}

/// Gradient roles (§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GradientRole {
    /// `temp_*`.
    Latency,
    /// `cpu_*`.
    Border,
    /// `download_*`.
    VuRemote,
    /// `upload_*`.
    VuMic,
    /// `used_*`.
    Unread,
    /// `process_*`.
    Selection,
    /// `free_*`.
    Free,
    /// `cached_*`.
    Cached,
    /// `available_*`.
    Available,
}

impl GradientRole {
    /// Every gradient role, in declaration order.
    pub const ALL: [Self; 9] = [
        Self::Latency,
        Self::Border,
        Self::VuRemote,
        Self::VuMic,
        Self::Unread,
        Self::Selection,
        Self::Free,
        Self::Cached,
        Self::Available,
    ];

    const fn idx(self) -> usize {
        self as usize
    }

    /// The RGB gradient backing this role in a palette.
    #[must_use]
    pub fn of(self, p: &bbs_core::ThemePalette) -> &ColorGradient {
        match self {
            Self::Latency => &p.temp_gradient,
            Self::Border => &p.cpu_gradient,
            Self::VuRemote => &p.download_gradient,
            Self::VuMic => &p.upload_gradient,
            Self::Unread => &p.used_gradient,
            Self::Selection => &p.process_gradient,
            Self::Free => &p.free_gradient,
            Self::Cached => &p.cached_gradient,
            Self::Available => &p.available_gradient,
        }
    }

    fn of_ansi16(self, a: &Ansi16Palette) -> Ansi16Gradient {
        match self {
            Self::Latency => a.temp_gradient,
            Self::Border => a.cpu_gradient,
            Self::VuRemote => a.download_gradient,
            Self::VuMic => a.upload_gradient,
            Self::Unread => a.used_gradient,
            Self::Selection => a.process_gradient,
            Self::Free => a.free_gradient,
            Self::Cached => a.cached_gradient,
            Self::Available => a.available_gradient,
        }
    }
}

type Lut<T> = [T; GRADIENT_STEPS];

/// A theme resolved for a colour depth.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    spec: ThemeSpec,
    depth: ColorDepth,
    /// `[ansi16]` table (explicit, `tty_default` for tty, or redmean-derived).
    ansi16: Ansi16Palette,
    /// Built-in `tty`: indexed 0..=15 at every colour depth.
    indexed_only: bool,
    /// OKLab RGB LUTs (depth-independent).
    rgb_luts: [Lut<RgbColor>; 9],
    /// Resolved role colours for `depth` (index = `Role as usize`).
    roles: [Color; 14],
    /// Resolved `selected_bg` for `depth`.
    selected_bg: Color,
    /// Resolved gradient LUTs for `depth`.
    luts: [Lut<Color>; 9],
}

impl Theme {
    /// Build a theme for `depth`, precomputing all role colours and gradient LUTs.
    #[must_use]
    pub fn new(spec: ThemeSpec, depth: ColorDepth) -> Self {
        let indexed_only = is_tty_spec(&spec);
        let ansi16 = if indexed_only {
            spec.palette
                .ansi16
                .clone()
                .unwrap_or_else(Ansi16Palette::tty_default)
        } else {
            spec.palette.resolved_ansi16()
        };
        let mut rgb_luts = [[RgbColor::default(); GRADIENT_STEPS]; 9];
        for g in GradientRole::ALL {
            rgb_luts[g.idx()] = oklab::gradient_lut(g.of(&spec.palette));
        }
        let mut t = Self {
            spec,
            depth,
            ansi16,
            indexed_only,
            rgb_luts,
            roles: [Color::Reset; 14],
            selected_bg: Color::Reset,
            luts: [[Color::Reset; GRADIENT_STEPS]; 9],
        };
        t.resolve();
        t
    }

    /// Change the colour depth (re-resolves the caches).
    pub fn set_depth(&mut self, depth: ColorDepth) {
        if depth != self.depth {
            self.depth = depth;
            self.resolve();
        }
    }

    /// Underlying spec.
    #[must_use]
    pub fn spec(&self) -> &ThemeSpec {
        &self.spec
    }
    /// Theme name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.spec.name
    }
    /// Active depth.
    #[must_use]
    pub fn depth(&self) -> ColorDepth {
        self.depth
    }
    /// `true` when this theme only ever emits `Color::Indexed(0..=15)` (built-in `tty`).
    #[must_use]
    pub fn is_indexed_only(&self) -> bool {
        self.indexed_only
    }
    /// The resolved ANSI-16 table used at `Ansi16` depth.
    #[must_use]
    pub fn ansi16(&self) -> &Ansi16Palette {
        &self.ansi16
    }

    /// Effective depth: `tty` behaves as `Ansi16` at every colour depth.
    fn eff_depth(&self) -> ColorDepth {
        match self.depth {
            ColorDepth::Monochrome => ColorDepth::Monochrome,
            _ if self.indexed_only => ColorDepth::Ansi16,
            d => d,
        }
    }

    /// Convert an arbitrary RGB (e.g. a Discord role colour) to the active depth.
    #[must_use]
    pub fn rgb(&self, c: RgbColor) -> Color {
        match self.eff_depth() {
            ColorDepth::TrueColor => Color::Rgb(c.r, c.g, c.b),
            ColorDepth::Ansi256 => Color::Indexed(c.to_ansi256()),
            ColorDepth::Ansi16 => Color::Indexed(c.to_ansi16()),
            ColorDepth::Monochrome => Color::Reset,
        }
    }

    fn resolve(&mut self) {
        let depth = self.eff_depth();
        let p = &self.spec.palette;
        let a = &self.ansi16;
        let pick = |rgb: RgbColor, idx: u8| -> Color {
            match depth {
                ColorDepth::TrueColor => Color::Rgb(rgb.r, rgb.g, rgb.b),
                ColorDepth::Ansi256 => Color::Indexed(rgb.to_ansi256()),
                ColorDepth::Ansi16 => Color::Indexed(idx & 0x0F),
                ColorDepth::Monochrome => Color::Reset,
            }
        };
        let bg = match (depth, p.main_bg, a.main_bg) {
            (ColorDepth::Monochrome, _, _) => Color::Reset,
            (ColorDepth::Ansi16, _, Some(i)) => Color::Indexed(i & 0x0F),
            (ColorDepth::Ansi16, _, None) | (_, None, _) => Color::Reset,
            (_, Some(c), _) => pick(c, 0),
        };
        for role in Role::ALL {
            self.roles[role.idx()] = match role {
                Role::Bg => bg,
                Role::Fg => pick(p.main_fg, a.main_fg),
                Role::Title => pick(p.title, a.title),
                Role::Hotkey => pick(p.hi_fg, a.hi_fg),
                Role::Selected => pick(p.selected_fg, a.selected_fg),
                Role::Inactive => pick(p.inactive_fg, a.inactive_fg),
                Role::GraphText => pick(p.graph_text, a.graph_text),
                Role::MeterBg => pick(p.meter_bg, a.meter_bg),
                Role::Mention => pick(p.proc_misc, a.proc_misc),
                Role::Divider => pick(p.div_line, a.div_line),
                Role::BoxSidebar => pick(p.cpu_box, a.cpu_box),
                Role::BoxChat => pick(p.mem_box, a.mem_box),
                Role::BoxVoice => pick(p.net_box, a.net_box),
                Role::BoxMembers => pick(p.proc_box, a.proc_box),
            };
        }
        self.selected_bg = pick(p.selected_bg, a.selected_bg);
        for g in GradientRole::ALL {
            let ag = g.of_ansi16(a);
            let rgb = &self.rgb_luts[g.idx()];
            for (i, slot) in self.luts[g.idx()].iter_mut().enumerate() {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / (GRADIENT_STEPS - 1) as f32;
                *slot = pick(rgb[i], ag.sample(t));
            }
        }
    }

    /// Background colour (`Reset` = transparent / terminal default).
    #[must_use]
    pub fn bg(&self) -> Color {
        self.roles[Role::Bg.idx()]
    }

    /// Foreground colour for a role (`Role::Bg` returns the background).
    #[must_use]
    pub fn color(&self, role: Role) -> Color {
        self.roles[role.idx()]
    }

    /// Resolved `selected_bg`.
    #[must_use]
    pub fn selected_bg(&self) -> Color {
        self.selected_bg
    }

    /// Full style for a role: fg = role colour, bg = theme bg (`selected_bg` for
    /// [`Role::Selected`]). In `Monochrome`: `Selected` = REVERSED, `Title` = BOLD,
    /// `Hotkey` = BOLD|UNDERLINED, `Inactive` = DIM.
    #[must_use]
    pub fn style(&self, role: Role) -> Style {
        let mut s = Style::default().fg(self.color(role)).bg(self.bg());
        if role == Role::Selected {
            s = s.bg(self.selected_bg);
        }
        if self.depth == ColorDepth::Monochrome {
            let m = match role {
                Role::Selected => Modifier::REVERSED,
                Role::Title => Modifier::BOLD,
                Role::Hotkey => Modifier::BOLD | Modifier::UNDERLINED,
                Role::Inactive => Modifier::DIM,
                _ => Modifier::empty(),
            };
            s = s.add_modifier(m);
        }
        s
    }

    /// Sample a gradient at `t ∈ [0,1]` (NaN → 0), quantized to 16 OKLab steps.
    #[must_use]
    pub fn gradient(&self, g: GradientRole, t: f32) -> Color {
        self.luts[g.idx()][oklab::quantize(t)]
    }

    /// RGB value of a gradient at `t` (same quantization; ignores depth).
    #[must_use]
    pub fn gradient_rgb(&self, g: GradientRole, t: f32) -> RgbColor {
        self.rgb_luts[g.idx()][oklab::quantize(t)]
    }

    /// Vertical positional border colour (§7): `t = row / (rows − 1)` on the
    /// [`GradientRole::Border`] gradient when `gradient_borders` is on, else the box `role` colour.
    #[must_use]
    pub fn border_color(&self, role: Role, row: u16, rows: u16, gradient_borders: bool) -> Color {
        if !gradient_borders || self.depth == ColorDepth::Monochrome {
            return self.color(role);
        }
        let t = if rows <= 1 {
            0.0
        } else {
            f32::from(row.min(rows - 1)) / f32::from(rows - 1)
        };
        self.gradient(GradientRole::Border, t)
    }

    /// Stable per-user nick colour from the hashed fallback palette (`free_*` / `cached_*` /
    /// `available_*`, §8.5): 3 gradients × 16 steps = 48 distinct slots.
    #[must_use]
    pub fn nick_color(&self, user_id: u64) -> Color {
        // splitmix64 finalizer: snowflakes share high bits (timestamp), so mix before bucketing.
        let mut z = user_id.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let g = [
            GradientRole::Free,
            GradientRole::Cached,
            GradientRole::Available,
        ][(z % 3) as usize];
        #[allow(clippy::cast_possible_truncation)]
        let step = ((z >> 8) % GRADIENT_STEPS as u64) as usize;
        self.luts[g.idx()][step]
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(ThemeSpec::default(), ColorDepth::TrueColor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPTHS: [ColorDepth; 4] = [
        ColorDepth::TrueColor,
        ColorDepth::Ansi256,
        ColorDepth::Ansi16,
        ColorDepth::Monochrome,
    ];

    fn all_colors(t: &Theme) -> Vec<Color> {
        let mut v: Vec<Color> = Role::ALL.iter().map(|r| t.color(*r)).collect();
        v.push(t.selected_bg());
        for g in GradientRole::ALL {
            for i in 0..=20 {
                v.push(t.gradient(g, i as f32 / 20.0));
            }
        }
        for row in 0..10 {
            v.push(t.border_color(Role::BoxChat, row, 10, true));
        }
        for id in 0..50u64 {
            v.push(t.nick_color(id * 7_919_000_000_017));
        }
        v
    }

    #[test]
    fn every_builtin_resolves_at_every_depth() {
        for spec in builtin_specs() {
            for d in DEPTHS {
                let t = Theme::new(spec.clone(), d);
                for c in all_colors(&t) {
                    match (d, t.is_indexed_only(), c) {
                        (ColorDepth::Monochrome, _, Color::Reset) => {}
                        (ColorDepth::TrueColor, false, Color::Rgb(..)) => {}
                        (ColorDepth::Ansi256, false, Color::Indexed(i)) => assert!(i >= 16),
                        (ColorDepth::Ansi16, _, Color::Indexed(i))
                        | (_, true, Color::Indexed(i)) => {
                            assert!(i <= 15);
                        }
                        other => panic!("{} at {d:?}: unexpected {other:?}", spec.name),
                    }
                }
                // Text is never the same colour as the background (except Monochrome).
                if d != ColorDepth::Monochrome {
                    assert_ne!(t.color(Role::Fg), t.bg(), "{} {d:?}", spec.name);
                    assert_ne!(
                        t.color(Role::Selected),
                        t.selected_bg(),
                        "{} {d:?}",
                        spec.name
                    );
                }
            }
        }
    }

    #[test]
    fn tty_is_always_indexed() {
        let spec = builtin_by_name("tty").unwrap();
        for d in [
            ColorDepth::TrueColor,
            ColorDepth::Ansi256,
            ColorDepth::Ansi16,
        ] {
            let t = Theme::new(spec.clone(), d);
            assert!(t.is_indexed_only());
            assert_eq!(t.bg(), Color::Indexed(0));
            assert_eq!(t.color(Role::Fg), Color::Indexed(7));
            assert_eq!(t.color(Role::Hotkey), Color::Indexed(14));
            assert_eq!(t.gradient(GradientRole::Border, 0.0), Color::Indexed(4));
            assert_eq!(t.gradient(GradientRole::Border, 0.5), Color::Indexed(6));
            assert_eq!(t.gradient(GradientRole::Border, 1.0), Color::Indexed(15));
            assert!(matches!(t.rgb(RgbColor::new(1, 200, 3)), Color::Indexed(i) if i <= 15));
            for c in all_colors(&t) {
                assert!(matches!(c, Color::Indexed(0..=15)), "{c:?} at {d:?}");
            }
        }
    }

    #[test]
    fn gradient_endpoints_and_quantization() {
        let spec = builtin_by_name("Gruvbox").unwrap();
        let t = Theme::new(spec, ColorDepth::TrueColor);
        assert_eq!(
            t.gradient(GradientRole::Border, 0.0),
            Color::Rgb(0xB8, 0xBB, 0x26)
        );
        assert_eq!(
            t.gradient(GradientRole::Border, 1.0),
            Color::Rgb(0xFB, 0x49, 0x34)
        );
        assert_eq!(
            t.gradient(GradientRole::Border, f32::NAN),
            t.gradient(GradientRole::Border, 0.0)
        );
        assert_eq!(
            t.gradient(GradientRole::Border, 9.0),
            t.gradient(GradientRole::Border, 1.0)
        );
        let mut distinct: Vec<Color> = (0..=1000)
            .map(|i| t.gradient(GradientRole::Border, i as f32 / 1000.0))
            .collect();
        distinct.dedup();
        assert!(distinct.len() <= GRADIENT_STEPS);
        assert!(distinct.len() >= 10);
        // Borders over 100 rows change colour at most 15 times.
        let changes = (1..100u16)
            .filter(|r| {
                t.border_color(Role::BoxChat, *r, 100, true)
                    != t.border_color(Role::BoxChat, r - 1, 100, true)
            })
            .count();
        assert!(changes < GRADIENT_STEPS);
        assert_eq!(
            t.border_color(Role::BoxChat, 3, 10, false),
            t.color(Role::BoxChat)
        );
        assert_eq!(
            t.border_color(Role::BoxChat, 0, 1, true),
            t.gradient(GradientRole::Border, 0.0)
        );
        assert_eq!(
            t.border_color(Role::BoxChat, 9, 10, true),
            t.gradient(GradientRole::Border, 1.0)
        );
    }

    #[test]
    fn monochrome_styles() {
        let t = Theme::new(ThemeSpec::default(), ColorDepth::Monochrome);
        let sel = t.style(Role::Selected);
        assert!(sel.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(sel.fg, Some(Color::Reset));
        assert!(t.style(Role::Title).add_modifier.contains(Modifier::BOLD));
        assert!(
            t.style(Role::Hotkey)
                .add_modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert!(t.style(Role::Inactive).add_modifier.contains(Modifier::DIM));
        assert!(t.style(Role::Fg).add_modifier.is_empty());
    }

    #[test]
    fn selected_style_uses_selected_bg_and_set_depth() {
        let mut t = Theme::new(builtin_by_name("nord").unwrap(), ColorDepth::TrueColor);
        assert_eq!(
            t.style(Role::Selected).bg,
            Some(Color::Rgb(0x43, 0x4C, 0x5E))
        );
        assert_eq!(t.style(Role::Fg).bg, Some(Color::Rgb(0x2E, 0x34, 0x40)));
        t.set_depth(ColorDepth::Ansi16);
        assert_eq!(t.depth(), ColorDepth::Ansi16);
        assert_eq!(t.style(Role::Selected).bg, Some(Color::Indexed(8)));
        assert_eq!(
            t,
            Theme::new(builtin_by_name("nord").unwrap(), ColorDepth::Ansi16)
        );
    }

    #[test]
    fn transparent_bg_and_imports_derive_ansi16() {
        let spec =
            parse_theme_file("m", "theme[main_bg]=\"\"\ntheme[main_fg]=\"#ff5555\"\n").unwrap();
        for d in DEPTHS {
            assert_eq!(Theme::new(spec.clone(), d).bg(), Color::Reset);
        }
        assert_eq!(
            Theme::new(spec, ColorDepth::Ansi16).color(Role::Fg),
            Color::Indexed(9)
        );
    }

    #[test]
    fn nick_colors_are_stable_and_varied() {
        let t = Theme::default();
        assert_eq!(t.nick_color(42), t.nick_color(42));
        let mut v: Vec<Color> = (0..200u64)
            .map(|i| t.nick_color(1_000_000_000_000 + i))
            .collect();
        v.sort_by_key(|c| format!("{c:?}"));
        v.dedup();
        assert!(v.len() > 12);
    }
}
