//! Two-axis geometry engine (§2): H-profile from `cols`, V-profile from `rows`, hysteresis,
//! the per-profile pane solver, chat measure and vertical budgets.
//!
//! Every function here is total: any `(cols, rows)` including `0×0` yields a valid layout whose
//! rects lie inside the frame (I1) and whose widths + border columns sum to `cols` (I2).

use bbs_core::LayoutProfile;
use ratatui::layout::Rect;

/// Sidebar width cap in Ultra (and the clamp max elsewhere).
pub const ULTRA_SIDEBAR: u16 = 32;
/// Members width in Ultra.
pub const ULTRA_MEMBERS: u16 = 30;
/// Minimum Ultra deck width.
pub const DECK_MIN: u16 = 56;
/// Ultra deck growth cap before all decks exist.
pub const DECK_CAP: u16 = 120;
/// Ultra chat pane cap (gutter 21 + measure 116 + scrollbar 1).
pub const CHAT_CAP: u16 = 138;
/// Maximum readable text measure (§2.3).
pub const TEXT_MEASURE_MAX: u16 = 116;
/// IRC-style gutter width: `HH:MM␠` + 12-cell nick + `␠│␠`.
pub const GUTTER: u16 = 21;

/// Horizontal profile, chosen from `cols` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HProfile {
    /// `< 24` cols.
    Micro,
    /// `24..=59` cols.
    Compact,
    /// `60..=139` cols.
    Standard,
    /// `140..=239` cols.
    Wide,
    /// `>= 240` cols.
    Ultra,
}

/// Vertical chrome profile, chosen from `rows` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VProfile {
    /// `<= 4` rows: pager.
    Micro,
    /// `5..=11` rows: borderless, reverse-video header.
    Short,
    /// `>= 12` rows: bordered frame with embedded titles/status.
    Tall,
}

const H_THRESHOLDS: [u16; 4] = [24, 60, 140, 240];
const V_THRESHOLDS: [u16; 2] = [5, 12];
const H_ORDER: [HProfile; 5] = [
    HProfile::Micro,
    HProfile::Compact,
    HProfile::Standard,
    HProfile::Wide,
    HProfile::Ultra,
];
const V_ORDER: [VProfile; 3] = [VProfile::Micro, VProfile::Short, VProfile::Tall];

fn h_index(p: HProfile) -> usize {
    H_ORDER.iter().position(|x| *x == p).unwrap_or(0)
}

fn v_index(p: VProfile) -> usize {
    V_ORDER.iter().position(|x| *x == p).unwrap_or(0)
}

impl HProfile {
    /// Raw classification without hysteresis.
    #[must_use]
    pub fn raw(cols: u16) -> Self {
        let idx = H_THRESHOLDS.iter().filter(|t| cols >= **t).count();
        H_ORDER[idx]
    }

    /// Classification with hysteresis (§2.1): downgrade immediately, upgrade only once every
    /// crossed threshold is cleared by `+2` cols.
    #[must_use]
    pub fn classify(cols: u16, prev: Option<Self>) -> Self {
        let raw = Self::raw(cols);
        let Some(prev) = prev else { return raw };
        let (ri, pi) = (h_index(raw), h_index(prev));
        if ri <= pi {
            return raw;
        }
        // Highest profile whose threshold is cleared by +2, but never below prev.
        let cleared = H_THRESHOLDS
            .iter()
            .filter(|t| cols >= t.saturating_add(2))
            .count();
        H_ORDER[cleared.max(pi).min(ri)]
    }
}

impl VProfile {
    /// Raw classification without hysteresis.
    #[must_use]
    pub fn raw(rows: u16) -> Self {
        let idx = V_THRESHOLDS.iter().filter(|t| rows >= **t).count();
        V_ORDER[idx]
    }

    /// Classification with hysteresis: upgrade needs `+1` row beyond the threshold.
    #[must_use]
    pub fn classify(rows: u16, prev: Option<Self>) -> Self {
        let raw = Self::raw(rows);
        let Some(prev) = prev else { return raw };
        let (ri, pi) = (v_index(raw), v_index(prev));
        if ri <= pi {
            return raw;
        }
        let cleared = V_THRESHOLDS
            .iter()
            .filter(|t| rows >= t.saturating_add(1))
            .count();
        V_ORDER[cleared.max(pi).min(ri)]
    }
}

/// Combine H and V into the core [`LayoutProfile`] label.
#[must_use]
pub fn profile_label(h: HProfile, v: VProfile) -> LayoutProfile {
    match (h, v) {
        (HProfile::Micro, _) | (_, VProfile::Micro) => LayoutProfile::Micro16x2,
        (HProfile::Compact, _) | (_, VProfile::Short) => LayoutProfile::Compact,
        (HProfile::Standard, _) => LayoutProfile::StandardBbs,
        (HProfile::Wide, _) => LayoutProfile::Wide,
        (HProfile::Ultra, _) => LayoutProfile::UltraWideCockpit,
    }
}

/// Hysteresis memory carried between frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutMemory {
    /// Previous H-profile.
    pub h: Option<HProfile>,
    /// Previous V-profile.
    pub v: Option<VProfile>,
    /// Gutter mode was on.
    pub gutter: bool,
    /// Side-car column was on.
    pub sidecar: bool,
}

/// Inputs that shape the layout besides the terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutInput {
    /// Terminal columns.
    pub cols: u16,
    /// Terminal rows.
    pub rows: u16,
    /// Lines the composer content wants (≥1).
    pub composer_lines: u16,
    /// Active banners (ring, attachment staging) in priority order count.
    pub banners: u16,
    /// Sidebar enabled (user toggle).
    pub show_sidebar: bool,
    /// Members list enabled (user toggle; only Wide/Ultra show it inline).
    pub show_members: bool,
    /// Maximum decks the user wants (0..=3).
    pub max_decks: u8,
}

impl LayoutInput {
    /// Defaults for a terminal size.
    #[must_use]
    pub const fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            composer_lines: 1,
            banners: 0,
            show_sidebar: true,
            show_members: true,
            max_decks: 3,
        }
    }
}

/// Text measure inside the chat pane (§2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatMeasure {
    /// Gutter width (21 or 0).
    pub gutter: u16,
    /// Body indent in header mode (2) else 0.
    pub indent: u16,
    /// Text measure `T`.
    pub text: u16,
    /// Side-car width (0 when absent); excludes the 1-col gap.
    pub sidecar: u16,
    /// Spare columns (`C − G − 1 − indent − T`).
    pub spare: u16,
}

impl ChatMeasure {
    /// Compute for chat inner width `c` with hysteresis memory.
    #[must_use]
    pub fn compute(c: u16, mem: &mut LayoutMemory) -> Self {
        let gutter_on = if mem.gutter { c >= 72 } else { c >= 74 };
        mem.gutter = gutter_on;
        let gutter = if gutter_on { GUTTER } else { 0 };
        let indent = if gutter_on { 0 } else { 2 };
        let avail = c.saturating_sub(gutter + 1 + indent);
        let text = avail.min(TEXT_MEASURE_MAX).max(1.min(c));
        let spare = avail.saturating_sub(text);
        let sidecar_on = if mem.sidecar {
            spare >= 23
        } else {
            spare >= 25
        };
        mem.sidecar = sidecar_on;
        let sidecar = if sidecar_on { spare - 1 } else { 0 };
        Self {
            gutter,
            indent,
            text,
            sidecar,
            spare,
        }
    }
}

/// Which pane a horizontal slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneKind {
    /// Guild/channel tree.
    Sidebar,
    /// Messages + composer column.
    Chat,
    /// Member list.
    Members,
    /// Ultra deck slot `0..=2` (content decided by call state).
    Deck(u8),
}

/// A horizontal pane slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pane {
    /// Kind.
    pub kind: PaneKind,
    /// Full-height rect of the pane's inner area.
    pub rect: Rect,
}

/// Fully solved frame layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Whole frame.
    pub area: Rect,
    /// H-profile.
    pub h: HProfile,
    /// V-profile.
    pub v: VProfile,
    /// Core label.
    pub profile: LayoutProfile,
    /// Pager mode (`H == Micro || V == Micro`).
    pub micro: bool,
    /// Outer frame columns (2 in Tall bordered layouts, else 0).
    pub outer: u16,
    /// Panes left→right.
    pub panes: Vec<Pane>,
    /// Divider x positions (between panes).
    pub dividers: Vec<u16>,
    /// Short-chrome header row.
    pub header: Option<Rect>,
    /// Tall top border row.
    pub top_border: Option<Rect>,
    /// Tall bottom border (status) row.
    pub bottom_border: Option<Rect>,
    /// Messages viewport.
    pub messages: Rect,
    /// Banner rows (stolen from messages).
    pub banners: Vec<Rect>,
    /// Separator row above the composer (`╟─╢` in Tall, `─` rule in tall-narrow).
    pub separator: Option<Rect>,
    /// Composer rows.
    pub composer: Rect,
    /// Chat measure.
    pub measure: ChatMeasure,
}

impl Layout {
    /// Rect of the first pane of `kind`.
    #[must_use]
    pub fn pane(&self, kind: PaneKind) -> Option<Rect> {
        self.panes.iter().find(|p| p.kind == kind).map(|p| p.rect)
    }

    /// Chat pane rect (full height).
    #[must_use]
    pub fn chat(&self) -> Rect {
        self.pane(PaneKind::Chat).unwrap_or(self.messages)
    }

    /// Deck rects in priority order.
    #[must_use]
    pub fn decks(&self) -> Vec<Rect> {
        self.panes
            .iter()
            .filter(|p| matches!(p.kind, PaneKind::Deck(_)))
            .map(|p| p.rect)
            .collect()
    }
}

fn round_half_up(x: f32) -> u16 {
    // Inputs are small non-negative values; saturate defensively.
    let v = (x + 0.5).floor();
    if v <= 0.0 {
        0
    } else if v >= f32::from(u16::MAX) {
        u16::MAX
    } else {
        v as u16
    }
}

/// Ultra greedy deck fill (§2.2). `avail` = width for chat + decks (each deck carries its own
/// divider). Returns `(chat, decks)`.
#[must_use]
pub fn ultra_fill(avail: u16, max_decks: u8) -> (u16, Vec<u16>) {
    let max_decks = usize::from(max_decks.min(3));
    let avail = i32::from(avail);
    if max_decks == 0 || avail < i32::from(DECK_MIN) + 1 + 24 {
        return (u16::try_from(avail.max(0)).unwrap_or(0), Vec::new());
    }
    let step = i32::from(DECK_MIN) + 1;
    let chat = i32::from(CHAT_CAP).min(avail - step);
    let mut rem = avail - chat - step;
    let mut decks: Vec<i32> = vec![i32::from(DECK_MIN)];
    let grow = |d: &mut i32, rem: &mut i32| {
        let g = (*rem).min(i32::from(DECK_CAP) - *d).max(0);
        *d += g;
        *rem -= g;
    };
    if let Some(d) = decks.last_mut() {
        grow(d, &mut rem);
    }
    while decks.len() < max_decks && rem >= step {
        rem -= step;
        decks.push(i32::from(DECK_MIN));
        if let Some(d) = decks.last_mut() {
            grow(d, &mut rem);
        }
    }
    let mut chat = chat;
    if decks.len() == 3 || decks.len() == max_decks {
        if decks.len() == 3 {
            let n = 3;
            let q = rem / n;
            let r = rem % n;
            for (i, d) in decks.iter_mut().enumerate() {
                *d += q + i32::from(i32::try_from(i).unwrap_or(0) < r);
            }
        } else {
            chat += rem;
        }
    } else {
        chat += rem;
    }
    let to16 = |v: i32| u16::try_from(v.max(0)).unwrap_or(u16::MAX);
    (to16(chat), decks.into_iter().map(to16).collect())
}

/// Solve horizontal pane widths for a profile. Returns widths in left→right order with kinds.
fn solve_widths(h: HProfile, cols: u16, outer: u16, input: &LayoutInput) -> Vec<(PaneKind, u16)> {
    let c = cols;
    let cf = f32::from(c);
    match h {
        HProfile::Micro | HProfile::Compact => vec![(PaneKind::Chat, c.saturating_sub(outer))],
        HProfile::Standard => {
            if input.show_sidebar {
                let s = round_half_up(0.22 * cf).clamp(18, 28);
                let chat = c.saturating_sub(outer + 1 + s);
                vec![(PaneKind::Sidebar, s), (PaneKind::Chat, chat)]
            } else {
                vec![(PaneKind::Chat, c.saturating_sub(outer))]
            }
        }
        HProfile::Wide => {
            let s = round_half_up(0.15 * cf).clamp(24, 32);
            let m = round_half_up(0.14 * cf).clamp(22, 30);
            let mut used = outer;
            let mut out = Vec::new();
            if input.show_sidebar {
                out.push((PaneKind::Sidebar, s));
                used += s + 1;
            }
            let members = input.show_members;
            if members {
                used += m + 1;
            }
            out.push((PaneKind::Chat, c.saturating_sub(used)));
            if members {
                out.push((PaneKind::Members, m));
            }
            out
        }
        HProfile::Ultra => {
            let mut fixed = outer;
            if input.show_sidebar {
                fixed += ULTRA_SIDEBAR + 1;
            }
            if input.show_members {
                fixed += ULTRA_MEMBERS + 1;
            }
            let avail = c.saturating_sub(fixed);
            let (chat, decks) = ultra_fill(avail, input.max_decks);
            let mut out = Vec::new();
            if input.show_sidebar {
                out.push((PaneKind::Sidebar, ULTRA_SIDEBAR));
            }
            out.push((PaneKind::Chat, chat));
            for (i, d) in decks.into_iter().enumerate() {
                out.push((PaneKind::Deck(u8::try_from(i).unwrap_or(0)), d));
            }
            if input.show_members {
                out.push((PaneKind::Members, ULTRA_MEMBERS));
            }
            out
        }
    }
}

/// Solve the full layout. `mem` carries hysteresis state between calls.
#[must_use]
pub fn solve(input: &LayoutInput, mem: &mut LayoutMemory) -> Layout {
    let cols = input.cols;
    let rows = input.rows;
    let area = Rect::new(0, 0, cols, rows);
    let h = HProfile::classify(cols, mem.h);
    let v = VProfile::classify(rows, mem.v);
    mem.h = Some(h);
    mem.v = Some(v);
    let profile = profile_label(h, v);
    let micro = h == HProfile::Micro || v == VProfile::Micro;

    let empty = Rect::new(0, 0, 0, 0);
    if micro {
        return Layout {
            area,
            h,
            v,
            profile,
            micro,
            outer: 0,
            panes: vec![Pane {
                kind: PaneKind::Chat,
                rect: area,
            }],
            dividers: Vec::new(),
            header: None,
            top_border: None,
            bottom_border: None,
            messages: area,
            banners: Vec::new(),
            separator: None,
            composer: empty,
            measure: ChatMeasure {
                text: cols.max(1),
                ..ChatMeasure::default()
            },
        };
    }

    let bordered = v == VProfile::Tall && h >= HProfile::Standard;
    let outer = if bordered { 2 } else { 0 };
    let widths = solve_widths(h, cols, outer, input);

    // Vertical extent of panes.
    let (pane_y, pane_h) = if bordered {
        (1, rows.saturating_sub(2))
    } else {
        (1, rows.saturating_sub(1))
    };

    let mut panes = Vec::new();
    let mut dividers = Vec::new();
    let mut x = outer / 2;
    for (i, (kind, w)) in widths.iter().enumerate() {
        if i > 0 {
            dividers.push(x);
            x += 1;
        }
        panes.push(Pane {
            kind: *kind,
            rect: Rect::new(x, pane_y, *w, pane_h),
        });
        x = x.saturating_add(*w);
    }

    let chat = panes
        .iter()
        .find(|p| p.kind == PaneKind::Chat)
        .map_or(Rect::new(0, pane_y, cols, pane_h), |p| p.rect);

    // Vertical budget inside the chat column.
    let tall_narrow = v == VProfile::Tall && !bordered;
    let (cmp_max, fixed_rows) = if bordered {
        ((rows / 4).min(6), 3u16)
    } else if tall_narrow {
        (((rows.saturating_sub(1)) / 4).min(6), 2u16)
    } else {
        ((rows.saturating_sub(1) / 4).max(1), 1u16)
    };
    let cmp = input.composer_lines.clamp(1, cmp_max.max(1));
    let base_m = rows.saturating_sub(fixed_rows + cmp);
    let banners_n = input.banners.min(base_m.saturating_sub(2));
    let m = base_m.saturating_sub(banners_n);

    let msg_y = pane_y;
    let messages = Rect::new(chat.x, msg_y, chat.width, m);
    let mut y = msg_y + m;
    let mut banners = Vec::new();
    for _ in 0..banners_n {
        banners.push(Rect::new(chat.x, y, chat.width, 1));
        y += 1;
    }
    let separator = if bordered || tall_narrow {
        let r = Rect::new(chat.x, y, chat.width, 1);
        y += 1;
        Some(r)
    } else {
        None
    };
    let composer = Rect::new(chat.x, y, chat.width, cmp.min(rows.saturating_sub(y)));

    let (header, top_border, bottom_border) = if bordered {
        (
            None,
            Some(Rect::new(0, 0, cols, 1.min(rows))),
            Some(Rect::new(0, rows.saturating_sub(1), cols, 1.min(rows))),
        )
    } else {
        (Some(Rect::new(0, 0, cols, 1.min(rows))), None, None)
    };

    let measure = ChatMeasure::compute(chat.width, mem);

    Layout {
        area,
        h,
        v,
        profile,
        micro,
        outer,
        panes,
        dividers,
        header,
        top_border,
        bottom_border,
        messages,
        banners,
        separator,
        composer,
        measure,
    }
}

/// Modal placement result (§2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalRect {
    /// Outer rect (including border when boxed).
    pub rect: Rect,
    /// Borderless full-screen sheet mode.
    pub sheet: bool,
    /// Inner content rect.
    pub inner: Rect,
    /// Whether a hint row is available.
    pub hint_row: bool,
}

/// Place a modal of desired size in `area`.
#[must_use]
pub fn modal_rect(area: Rect, want_w: u16, want_h: u16) -> ModalRect {
    if area.width < 40 || area.height < 12 {
        return ModalRect {
            rect: area,
            sheet: true,
            inner: area,
            hint_row: area.height >= 7,
        };
    }
    let w = want_w
        .clamp(24, area.width.saturating_sub(4).max(24))
        .min(area.width);
    let h = want_h
        .clamp(5, area.height.saturating_sub(2).max(5))
        .min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let rect = Rect::new(x, y, w, h);
    let inner = Rect::new(x + 1, y + 1, w.saturating_sub(2), h.saturating_sub(2));
    ModalRect {
        rect,
        sheet: false,
        inner,
        hint_row: h >= 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(cols: u16, rows: u16) -> Vec<u16> {
        let mut mem = LayoutMemory::default();
        let l = solve(&LayoutInput::new(cols, rows), &mut mem);
        l.panes.iter().map(|p| p.rect.width).collect()
    }

    #[test]
    fn reference_widths_tall() {
        assert_eq!(widths(60, 24), vec![18, 39]);
        assert_eq!(widths(80, 24), vec![18, 59]);
        assert_eq!(widths(100, 24), vec![22, 75]);
        assert_eq!(widths(120, 24), vec![26, 91]);
        assert_eq!(widths(139, 24), vec![28, 108]);
        assert_eq!(widths(140, 24), vec![24, 90, 22]);
        assert_eq!(widths(160, 24), vec![24, 110, 22]);
        assert_eq!(widths(200, 24), vec![30, 138, 28]);
        assert_eq!(widths(239, 24), vec![32, 173, 30]);
        assert_eq!(widths(240, 24), vec![32, 117, 56, 30]);
        assert_eq!(widths(261, 24), vec![32, 138, 56, 30]);
        assert_eq!(widths(269, 24), vec![32, 138, 64, 30]);
        assert_eq!(widths(325, 24), vec![32, 138, 120, 30]);
        assert_eq!(widths(382, 24), vec![32, 138, 120, 56, 30]);
        assert_eq!(widths(503, 24), vec![32, 138, 120, 120, 56, 30]);
        assert_eq!(widths(512, 144), vec![32, 138, 120, 120, 65, 30]);
        assert_eq!(widths(640, 180), vec![32, 138, 145, 144, 144, 30]);
    }

    #[test]
    fn reference_widths_short() {
        assert_eq!(widths(80, 8), vec![18, 61]);
        assert_eq!(widths(200, 8), vec![30, 140, 28]);
        assert_eq!(widths(240, 8), vec![32, 119, 56, 30]);
        assert_eq!(widths(512, 8), vec![32, 138, 120, 120, 67, 30]);
    }

    #[test]
    fn i1_i2_full_grid() {
        for cols in 0..=700u16 {
            for rows in [0u16, 1, 2, 3, 4, 5, 6, 8, 11, 12, 13, 24, 60, 144, 200] {
                let mut mem = LayoutMemory::default();
                let l = solve(&LayoutInput::new(cols, rows), &mut mem);
                for p in &l.panes {
                    assert!(
                        p.rect.right() <= cols && p.rect.bottom() <= rows,
                        "{cols}x{rows} {p:?}"
                    );
                }
                if !l.micro {
                    let sum: u16 = l.panes.iter().map(|p| p.rect.width).sum::<u16>()
                        + u16::try_from(l.dividers.len()).unwrap_or(0)
                        + l.outer;
                    assert_eq!(sum, cols, "{cols}x{rows}");
                    assert!(l.messages.bottom() <= rows);
                    assert!(l.composer.bottom() <= rows);
                }
            }
        }
    }

    #[test]
    fn i3_ultra_decks_monotone() {
        for rows in [8u16, 60] {
            let mut prev: Vec<u16> = Vec::new();
            for cols in 240..=700u16 {
                let mut mem = LayoutMemory {
                    h: Some(HProfile::Ultra),
                    v: None,
                    ..LayoutMemory::default()
                };
                let l = solve(&LayoutInput::new(cols, rows), &mut mem);
                let d: Vec<u16> = l.decks().iter().map(|r| r.width).collect();
                for (a, b) in prev.iter().zip(d.iter()) {
                    assert!(b >= a, "cols {cols}: {prev:?} -> {d:?}");
                }
                prev = d;
            }
        }
    }

    #[test]
    fn hysteresis_upgrade_band() {
        assert_eq!(
            HProfile::classify(140, Some(HProfile::Standard)),
            HProfile::Standard
        );
        assert_eq!(
            HProfile::classify(141, Some(HProfile::Standard)),
            HProfile::Standard
        );
        assert_eq!(
            HProfile::classify(142, Some(HProfile::Standard)),
            HProfile::Wide
        );
        assert_eq!(
            HProfile::classify(139, Some(HProfile::Wide)),
            HProfile::Standard
        );
        assert_eq!(
            HProfile::classify(25, Some(HProfile::Micro)),
            HProfile::Micro
        );
        assert_eq!(
            HProfile::classify(700, Some(HProfile::Micro)),
            HProfile::Ultra
        );
        assert_eq!(
            VProfile::classify(5, Some(VProfile::Micro)),
            VProfile::Micro
        );
        assert_eq!(
            VProfile::classify(6, Some(VProfile::Micro)),
            VProfile::Short
        );
        assert_eq!(
            VProfile::classify(12, Some(VProfile::Short)),
            VProfile::Short
        );
        assert_eq!(
            VProfile::classify(13, Some(VProfile::Short)),
            VProfile::Tall
        );
    }

    #[test]
    fn vertical_budgets() {
        let mut mem = LayoutMemory::default();
        let l = solve(&LayoutInput::new(80, 24), &mut mem);
        assert_eq!(l.messages.height, 20);
        let mut mem = LayoutMemory::default();
        let l = solve(&LayoutInput::new(24, 5), &mut mem);
        assert_eq!((l.messages.height, l.composer.height), (3, 1));
        let mut mem = LayoutMemory::default();
        let mut inp = LayoutInput::new(80, 12);
        inp.composer_lines = 9;
        inp.banners = 5;
        let l = solve(&inp, &mut mem);
        assert_eq!(l.composer.height, 3);
        assert!(l.messages.height >= 2);
    }

    #[test]
    fn measure_matches_spec() {
        let mut mem = LayoutMemory::default();
        let m = ChatMeasure::compute(173, &mut mem);
        assert_eq!((m.gutter, m.text, m.spare, m.sidecar), (21, 116, 35, 34));
        let mut mem = LayoutMemory::default();
        let m = ChatMeasure::compute(59, &mut mem);
        assert_eq!((m.gutter, m.indent, m.text, m.spare), (0, 2, 56, 0));
        let mut mem = LayoutMemory {
            gutter: true,
            ..LayoutMemory::default()
        };
        assert_eq!(ChatMeasure::compute(72, &mut mem).gutter, 21);
        let mut mem = LayoutMemory::default();
        assert_eq!(ChatMeasure::compute(73, &mut mem).gutter, 0);
    }

    #[test]
    fn modal_sheet_on_small() {
        let m = modal_rect(Rect::new(0, 0, 24, 5), 60, 20);
        assert!(m.sheet);
        assert!(!m.hint_row);
        let m = modal_rect(Rect::new(0, 0, 80, 24), 60, 20);
        assert!(!m.sheet);
        assert_eq!(m.rect.width, 60);
    }
}
