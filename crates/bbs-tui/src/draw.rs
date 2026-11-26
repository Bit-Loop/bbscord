//! Low-level drawing helpers shared by every widget: clipped text, fills, boxes, lists,
//! scrollbars and the btop-style frame chrome (§7).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::charset::Glyphs;
use crate::config::TuiConfig;
use crate::text::{self, TextCtx, str_width};
use crate::theme::{Role, Theme};

/// Everything a widget needs to draw.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    /// Theme.
    pub theme: &'a Theme,
    /// Glyphs.
    pub g: Glyphs,
    /// Config.
    pub cfg: &'a TuiConfig,
    /// Text context.
    pub text: TextCtx,
    /// Current time (unix ms).
    pub now_ms: u64,
}

impl Ctx<'_> {
    /// Style for a role on the base background.
    #[must_use]
    pub fn s(&self, role: Role) -> Style {
        self.theme.style(role)
    }

    /// Base style.
    #[must_use]
    pub fn base(&self) -> Style {
        self.theme.style(Role::Fg)
    }

    /// Normalize a (trusted, internal) label for the charset.
    #[must_use]
    pub fn n(&self, s: &str) -> String {
        text::normalize(s, &self.text).into_owned()
    }
}

/// Fill `area` with spaces in `style`.
pub fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_symbol(" ").set_style(style);
        }
    }
}

/// Clipped text; returns cells written. Never panics for out-of-range coordinates.
pub fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, max_w: u16, style: Style) -> u16 {
    if max_w == 0 {
        return 0;
    }
    text::put_str_clipped(buf, x, y, s, max_w, style)
}

/// Set a single cell symbol if inside the buffer.
pub fn cell(buf: &mut Buffer, x: u16, y: u16, sym: &str, style: Style) {
    let a = buf.area;
    if x >= a.left() && x < a.right() && y >= a.top() && y < a.bottom() {
        buf[(x, y)].set_symbol(sym).set_style(style);
    }
}

/// Draw text padded/truncated to exactly `w` cells.
pub fn put_fit(buf: &mut Buffer, x: u16, y: u16, s: &str, w: u16, style: Style) {
    let f = text::fit(s, usize::from(w));
    put(buf, x, y, &f, w, style);
}

/// Draw text right-aligned within `w` cells.
pub fn put_right(buf: &mut Buffer, x: u16, y: u16, s: &str, w: u16, style: Style) {
    let sw = u16::try_from(str_width(s)).unwrap_or(u16::MAX).min(w);
    put(buf, x + (w - sw), y, s, sw, style);
}

/// Boxed modal/overlay frame with an embedded title; returns the inner rect.
pub fn boxed(buf: &mut Buffer, ctx: &Ctx<'_>, area: Rect, title: &str, role: Role) -> Rect {
    let area = area.intersection(buf.area);
    if area.width < 2 || area.height < 2 {
        fill(buf, area, ctx.base());
        return area;
    }
    let g = ctx.g;
    let bs = ctx.s(role);
    fill(buf, area, ctx.base());
    let (l, r, t, b) = (area.left(), area.right() - 1, area.top(), area.bottom() - 1);
    for x in l + 1..r {
        cell(buf, x, t, g.h, bs);
        cell(buf, x, b, g.h, bs);
    }
    for y in t + 1..b {
        cell(buf, l, y, g.v, bs);
        cell(buf, r, y, g.v, bs);
    }
    cell(buf, l, t, g.tl, bs);
    cell(buf, r, t, g.tr, bs);
    cell(buf, l, b, g.bl, bs);
    cell(buf, r, b, g.br, bs);
    if !title.is_empty() && area.width > 6 {
        let max_w = usize::from(area.width - 4);
        let chrome_w = str_width(g.title_l) + str_width(g.title_r);
        let inner = text::truncate(&ctx.n(title), max_w.saturating_sub(chrome_w), g.ellipsis);
        let tt = format!("{}{inner}{}", g.title_l, g.title_r);
        put(
            buf,
            l + 2,
            t,
            &tt,
            area.width - 4,
            ctx.s(Role::Title).add_modifier(Modifier::BOLD),
        );
    }
    text::blank_orphans(buf, area);
    Rect::new(l + 1, t + 1, area.width - 2, area.height - 2)
}

#[allow(clippy::too_many_arguments)]
/// Vertical scrollbar in column `x` over `rows` rows starting at `y`.
pub fn scrollbar(
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
    x: u16,
    y: u16,
    rows: u16,
    above: usize,
    visible: usize,
    below: usize,
) {
    if rows == 0 {
        return;
    }
    let total = above + visible + below;
    let track = ctx.s(Role::Inactive);
    let thumb = ctx.s(Role::Hotkey);
    if total <= visible || total == 0 {
        for r in 0..rows {
            cell(buf, x, y + r, " ", ctx.base());
        }
        return;
    }
    let rows_f = f32::from(rows);
    let th = ((visible as f32 / total as f32) * rows_f)
        .ceil()
        .clamp(1.0, rows_f) as u16;
    let start = ((above as f32 / total as f32) * rows_f).floor() as u16;
    let start = start.min(rows - th);
    for r in 0..rows {
        let (sym, st) = if r >= start && r < start + th {
            (ctx.g.thumb, thumb)
        } else {
            (ctx.g.track, track)
        };
        cell(buf, x, y + r, sym, st);
    }
}

#[allow(clippy::too_many_arguments)]
/// A selectable list row: marker + label, selected rows in the selection style.
pub fn list_row(
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
    x: u16,
    y: u16,
    w: u16,
    label: &str,
    selected: bool,
    style: Style,
) {
    if w == 0 {
        return;
    }
    let (marker, st) = if selected {
        (ctx.g.cursor, ctx.s(Role::Selected))
    } else {
        (" ", style)
    };
    let mw = u16::try_from(str_width(marker)).unwrap_or(1).min(w);
    let row = Rect::new(x, y, w, 1);
    fill(buf, row, if selected { st } else { ctx.base() });
    put(buf, x, y, marker, mw, st);
    put_fit(buf, x + mw, y, label, w - mw, st);
}

/// Colour of an outer/inner border cell at `row` of `rows` for a pane role.
#[must_use]
pub fn border_color(ctx: &Ctx<'_>, role: Role, row: u16, rows: u16) -> Color {
    ctx.theme
        .border_color(role, row, rows, ctx.cfg.gradient_borders)
}

/// Style for a border cell.
#[must_use]
pub fn border_style(ctx: &Ctx<'_>, role: Role, row: u16, rows: u16) -> Style {
    ctx.base().fg(border_color(ctx, role, row, rows))
}
