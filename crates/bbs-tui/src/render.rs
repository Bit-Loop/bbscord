//! Frame renderer: chrome (frame, embedded titles, btop status), chat viewport, composer,
//! banners, panes and modals.

use bbs_core::{CallRingState, ConnectionState, GraphicsTier, Snowflake};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use crate::app::{App, Focus, ImageSlot, Mode};
use crate::composer::ComposeTarget;
use crate::draw::{self, Ctx};
use crate::geometry::{self, Layout, LayoutInput, PaneKind};
use crate::markdown::{MdStyle, MentionKind};
use crate::messages::{LayoutParams, Seg, Sem};
use crate::meters;
use crate::sanitize::sanitize_single_line;
use crate::scroll::{Timeline, params_key};
use crate::state::Store;
use crate::text::{self, str_width};
use crate::theme::{GradientRole, Role};

/// Resolve a mention id to a display name.
#[must_use]
pub fn resolve_mention(store: &Store, k: MentionKind, id: u64) -> Option<String> {
    let sf = Snowflake::new(id);
    match k {
        MentionKind::User => store.user_name(sf),
        MentionKind::Channel => store.channel(sf).map(bbs_core::Channel::display_label),
        MentionKind::Role => None,
    }
}

/// Layout params for the active messages viewport `(params, key, height)`.
#[must_use]
pub fn messages_view_params(app: &App) -> Option<(LayoutParams, u64, u16)> {
    let l = app.layout.as_ref()?;
    if l.micro {
        return None;
    }
    let p = LayoutParams {
        width: l.messages.width,
        measure: l.measure,
        ctx: app.cfg.text_ctx(),
        glyphs: app.glyphs(),
        max_inline_cols: u8::try_from(l.profile.max_inline_embed_cols()).unwrap_or(1),
        cell_px: app.cfg.cell_px,
        max_img_rows: (l.messages.height / 2).clamp(1, 16),
        utc_offset_min: app.utc_offset_min,
        text_tags: app.cfg.graphics == GraphicsTier::TextTagOnly,
    };
    let k = params_key(&p);
    Some((p, k, l.messages.height))
}

impl App {
    /// Render into a ratatui frame (sets the cursor when composing).
    pub fn render(&mut self, f: &mut Frame<'_>) {
        let area = f.area();
        let cursor = self.render_buffer(area, f.buffer_mut());
        if let Some((x, y)) = cursor {
            f.set_cursor_position(Position::new(x, y));
        }
    }

    /// Render into a buffer; returns the text cursor position if any.
    pub fn render_buffer(&mut self, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
        self.size = (area.width, area.height);
        self.image_slots.clear();
        // Composer line demand depends on chat width, which depends on layout: solve twice cheaply.
        let mut input = LayoutInput::new(area.width, area.height);
        input.show_sidebar = self.show_sidebar;
        input.show_members = self.show_members;
        input.max_decks = self.cfg.max_decks;
        let h_probe = geometry::HProfile::classify(area.width, self.mem.h);
        let v_probe = geometry::VProfile::classify(area.height, self.mem.v);
        input.banners = self.banner_count(h_probe, v_probe);
        let mut mem = self.mem;
        let probe = geometry::solve(&input, &mut mem);
        let (lines, _, _) = self.composer.layout(probe.composer.width.max(2));
        input.composer_lines = u16::try_from(lines.len()).unwrap_or(1).max(1);
        let mut layout = geometry::solve(&input, &mut self.mem);
        // Area offset (frames are usually at 0,0 but be safe).
        offset_layout(&mut layout, area.x, area.y);
        self.layout = Some(layout.clone());

        let theme = self.theme.clone();
        let cfg = self.cfg.clone();
        let ctx = Ctx {
            theme: &theme,
            g: self.glyphs(),
            cfg: &cfg,
            text: cfg.text_ctx(),
            now_ms: self.now_ms,
        };
        draw::fill(buf, area, ctx.base());
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if layout.micro {
            let cursor = crate::micro::render(self, area, buf, &ctx);
            text::repair_wide_cells(buf);
            return cursor;
        }

        let bordered = layout.top_border.is_some();
        if bordered {
            self.draw_frame(&layout, buf, &ctx);
        } else {
            self.draw_short_chrome(&layout, buf, &ctx);
        }
        for p in layout.panes.clone() {
            match p.kind {
                PaneKind::Sidebar => crate::panes::render_sidebar(self, p.rect, buf, &ctx),
                PaneKind::Members => crate::panes::render_members(self, p.rect, buf, &ctx, false),
                PaneKind::Deck(i) => crate::panes::render_deck(self, i, p.rect, buf, &ctx),
                PaneKind::Chat => {}
            }
        }
        self.draw_messages(&layout, buf, &ctx);
        self.draw_banners(&layout, buf, &ctx);
        self.draw_separator(&layout, buf, &ctx);
        let mut cursor = self.draw_composer(&layout, buf, &ctx);
        if let Some(m) = self.modals.last().cloned() {
            let c = crate::modals::render(self, &m, area, buf, &ctx);
            cursor = if m.has_input() { c } else { None };
        }
        text::repair_wide_cells(buf);
        cursor
    }

    fn banner_count(&self, h: geometry::HProfile, v: geometry::VProfile) -> u16 {
        let bordered = v == geometry::VProfile::Tall && h >= geometry::HProfile::Standard;
        let ring = bordered && matches!(self.store.ring, CallRingState::IncomingRinging { .. });
        let dave_degraded = self
            .store
            .voice
            .as_ref()
            .is_some_and(|vc| matches!(vc.dave, bbs_core::DaveSessionState::Degraded { .. }));
        u16::from(ring)
            + u16::from(dave_degraded)
            + u16::from(!self.composer.attachments.is_empty())
    }

    fn pane_title(&self, kind: PaneKind, idx: u8, ctx: &Ctx<'_>) -> String {
        let n = ctx.g.pane_number(idx + 1);
        match kind {
            PaneKind::Sidebar => format!("{n}guilds"),
            PaneKind::Members => format!("{n}members"),
            PaneKind::Deck(i) => {
                let k = match crate::panes::deck_kind(self, i) {
                    crate::panes::DeckKind::Stage => "stage",
                    crate::panes::DeckKind::Media => "media",
                    crate::panes::DeckKind::Threads => "threads·pins",
                    crate::panes::DeckKind::Mentions => "mentions",
                };
                format!("{n}{k}")
            }
            PaneKind::Chat => {
                let name = self
                    .active_channel
                    .and_then(|c| self.store.channel(c))
                    .map(|c| {
                        let sig = if ctx.g.superscripts {
                            c.kind.utf8_sigil()
                        } else {
                            c.kind.ascii_sigil()
                        };
                        let topic = c
                            .topic
                            .as_deref()
                            .map(|t| format!(" {} {}", ctx.g.dot, sanitize_single_line(t)))
                            .unwrap_or_default();
                        format!("{sig}{}{topic}", sanitize_single_line(&c.display_label()))
                    })
                    .unwrap_or_else(|| "bbscord".to_string());
                format!("{n}{name}")
            }
        }
    }

    fn draw_frame(&self, l: &Layout, buf: &mut Buffer, ctx: &Ctx<'_>) {
        let g = ctx.g;
        let a = l.area;
        let rows = a.height;
        let top = a.y;
        let bot = a.bottom() - 1;
        let left = a.x;
        let right = a.right() - 1;
        // Top + bottom rows.
        let st_top = draw::border_style(ctx, Role::BoxChat, 0, rows);
        let st_bot = draw::border_style(ctx, Role::BoxChat, rows - 1, rows);
        for x in left..=right {
            draw::cell(buf, x, top, g.h, st_top);
            draw::cell(buf, x, bot, g.h, st_bot);
        }
        draw::cell(buf, left, top, g.tl, st_top);
        draw::cell(buf, right, top, g.tr, st_top);
        draw::cell(buf, left, bot, g.bl, st_bot);
        draw::cell(buf, right, bot, g.br, st_bot);
        for y in top + 1..bot {
            let st = draw::border_style(ctx, Role::BoxChat, y - top, rows);
            draw::cell(buf, left, y, g.v, st);
            draw::cell(buf, right, y, g.v, st);
            for d in &l.dividers {
                draw::cell(
                    buf,
                    *d,
                    y,
                    g.v,
                    draw::border_style(ctx, Role::Divider, y - top, rows),
                );
            }
        }
        for d in &l.dividers {
            draw::cell(buf, *d, top, g.t_down, st_top);
            draw::cell(buf, *d, bot, g.t_up, st_bot);
        }
        // Embedded titles.
        let title_style = ctx.s(Role::Title).add_modifier(Modifier::BOLD);
        for (i, p) in l.panes.iter().enumerate() {
            let t = self.pane_title(p.kind, u8::try_from(i).unwrap_or(0), ctx);
            let focused = match (p.kind, self.focus) {
                (PaneKind::Sidebar, Focus::Sidebar)
                | (PaneKind::Chat, Focus::Messages)
                | (PaneKind::Members, Focus::Members) => true,
                (PaneKind::Deck(a), Focus::Deck(b)) => a == b,
                _ => false,
            };
            if p.rect.width > 4 {
                let max_w = usize::from(p.rect.width - 2);
                let chrome_w = str_width(g.title_l) + str_width(g.title_r);
                let inner = text::truncate(&ctx.n(&t), max_w.saturating_sub(chrome_w), g.ellipsis);
                let s = format!("{}{inner}{}", g.title_l, g.title_r);
                let st = if focused {
                    ctx.s(Role::Hotkey).add_modifier(Modifier::BOLD)
                } else {
                    title_style
                };
                draw::put(buf, p.rect.x + 1, top, &s, p.rect.width - 2, st);
            }
        }
        text::blank_orphans(buf, Rect::new(left, top, a.width, 1));
        self.draw_status(
            l,
            Rect::new(left + 1, bot, a.width.saturating_sub(2), 1),
            buf,
            ctx,
            true,
        );
    }

    fn draw_short_chrome(&self, l: &Layout, buf: &mut Buffer, ctx: &Ctx<'_>) {
        let Some(h) = l.header else { return };
        let hs = ctx.s(Role::Selected).add_modifier(Modifier::BOLD);
        draw::fill(buf, h, hs);
        // Ring preempts the header.
        if let CallRingState::IncomingRinging { caller_name, .. } = &self.store.ring {
            let s = format!(
                "{} {} is calling - [a]nswer [h]ang up",
                ctx.g.phone,
                sanitize_single_line(caller_name)
            );
            draw::put(
                buf,
                h.x,
                h.y,
                &ctx.n(&s),
                h.width,
                hs.add_modifier(Modifier::REVERSED),
            );
            return;
        }
        let status_w = (h.width / 3).max(12).min(h.width);
        let status_x = h.right() - status_w;
        for (i, p) in l.panes.iter().enumerate() {
            let t = self.pane_title(p.kind, u8::try_from(i).unwrap_or(0), ctx);
            let w = p
                .rect
                .width
                .min(status_x.saturating_sub(p.rect.x).saturating_sub(1));
            draw::put(buf, p.rect.x, h.y, &ctx.n(&t), w, hs);
        }
        // Status at the right of the header (own cleared region).
        draw::fill(buf, Rect::new(status_x, h.y, status_w, 1), hs);
        let typing = self
            .active_channel
            .map(|c| self.store.typing_names(c, self.now_unix()))
            .unwrap_or_default();
        if typing.is_empty() {
            self.draw_status(l, Rect::new(status_x, h.y, status_w, 1), buf, ctx, false);
        } else {
            let who = if typing.len() > 2 {
                "several".to_string()
            } else {
                typing
                    .iter()
                    .map(|n| sanitize_single_line(n).into_owned())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            draw::put(
                buf,
                status_x + 1,
                h.y,
                &ctx.n(&format!("{who} typing…")),
                status_w.saturating_sub(1),
                hs,
            );
        }
        let pane_y = h.bottom();
        let rows = l.area.bottom().saturating_sub(pane_y);
        for d in &l.dividers {
            for y in pane_y..pane_y + rows {
                draw::cell(buf, *d, y, ctx.g.thin_v, ctx.s(Role::Divider));
            }
        }
    }

    /// Status segments (dropped right-to-left as space runs out).
    fn draw_status(&self, l: &Layout, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>, bordered: bool) {
        let g = ctx.g;
        let mut segs: Vec<(Vec<(String, Style)>, u8)> = Vec::new(); // (pieces, priority: lower = keep)
        // Latency sparkline.
        // Dynamic range: scale to the recent max (≥100 ms) so normal jitter is visible.
        let peak = self
            .store
            .latency
            .iter()
            .copied()
            .max()
            .unwrap_or(100)
            .max(100) as f32
            * 1.1;
        let lat: Vec<f32> = self
            .store
            .latency
            .iter()
            .map(|ms| (*ms as f32 / peak).clamp(0.0, 1.0))
            .collect();
        let conn = match self.store.connection {
            ConnectionState::Ready => String::new(),
            ConnectionState::Connecting => "connecting ".to_string(),
            ConnectionState::Resuming => "resuming ".to_string(),
            ConnectionState::Reconnecting => "reconnecting ".to_string(),
            ConnectionState::Disconnected => "offline ".to_string(),
        };
        let last = self.store.latency.back().copied();
        let spark_w = if ctx.cfg.low_bandwidth { 4 } else { 8 };
        let spark = meters::sparkline(&lat, spark_w, ctx.cfg.charset);
        let t = last.map_or(0.0, |ms| (ms as f32 / 400.0).clamp(0.0, 1.0));
        let mut pieces = vec![(conn, ctx.s(Role::Mention))];
        pieces.push((
            spark,
            ctx.base().fg(ctx.theme.gradient(GradientRole::Latency, t)),
        ));
        pieces.push((
            last.map(|ms| format!(" {ms}ms"))
                .unwrap_or_else(|| " --ms".to_string()),
            ctx.s(Role::GraphText),
        ));
        segs.push((pieces, 1));
        // Voice.
        if let Some(v) = &self.store.voice {
            let (dave, dave_role) =
                crate::panes::dave_badge(ctx, &v.dave, self.is_dave_all_verified());
            let head = format!(
                "{} {} {}",
                g.speaking,
                sanitize_single_line(&v.channel_name),
                v.participants.len(),
            );
            let head_st = if matches!(v.dave, bbs_core::DaveSessionState::Degraded { .. }) {
                ctx.s(Role::Mention)
            } else {
                ctx.s(Role::Hotkey)
            };
            let mut vpieces = vec![(head, head_st)];
            if !dave.is_empty() {
                vpieces.push((dave, ctx.s(dave_role)));
            }
            let flags = format!(
                "{}{}",
                if v.self_mute { " M" } else { "" },
                if v.self_deaf { " D" } else { "" }
            );
            if !flags.is_empty() {
                vpieces.push((flags, ctx.s(Role::Mention)));
            }
            segs.push((vpieces, 0));
        }
        // Unread.
        let (mut u, mut m) = self.store.dm_unread();
        for gld in &self.store.guilds {
            m += gld.mention_count;
            u += u32::from(gld.unread);
        }
        if u > 0 || m > 0 {
            segs.push((
                vec![(format!("@{m} {}{u}", g.unread), ctx.s(Role::Mention))],
                2,
            ));
        }
        // Mode + hotkey menu bar.
        let mode = match (self.mode, self.modals.is_empty()) {
            (_, false) => "MODAL".to_string(),
            (Mode::Insert, _) => "INSERT".to_string(),
            (Mode::Normal, _) => "NORMAL".to_string(),
        };
        segs.push((
            vec![(mode, ctx.s(Role::Title).add_modifier(Modifier::BOLD))],
            1,
        ));
        let menu = if l.chat().width >= 60 {
            "[k]jump [i]nsert [V]oice [T]heme [?]help [q]uit"
        } else {
            "[?]help"
        };
        let mut menu_pieces = Vec::new();
        for part in menu.split(' ') {
            // Colour the hotkey letter.
            if let (Some(a), Some(b)) = (part.find('['), part.find(']')) {
                menu_pieces.push((part[..a + 1].to_string(), ctx.s(Role::Inactive)));
                menu_pieces.push((
                    part[a + 1..b].to_string(),
                    ctx.s(Role::Hotkey).add_modifier(Modifier::BOLD),
                ));
                menu_pieces.push((format!("{} ", &part[b..]), ctx.s(Role::Inactive)));
            }
        }
        if let Some(last) = menu_pieces.last_mut() {
            last.0 = last.0.trim_end().to_string();
        }
        segs.push((menu_pieces, 3));
        // Drop lowest priority until it fits.
        let seg_w = |s: &(Vec<(String, Style)>, u8)| -> usize {
            s.0.iter().map(|p| str_width(&ctx.n(&p.0))).sum::<usize>()
                + if bordered { 4 } else { 3 }
        };
        while segs.iter().map(seg_w).sum::<usize>() > usize::from(area.width) && !segs.is_empty() {
            let worst = segs
                .iter()
                .enumerate()
                .max_by_key(|(i, s)| (s.1, *i))
                .map(|(i, _)| i)
                .unwrap_or(0);
            segs.remove(worst);
        }
        let mut x = area.x;
        let bstyle = draw::border_style(
            ctx,
            Role::BoxChat,
            l.area.height.saturating_sub(1),
            l.area.height,
        );
        for s in segs {
            if bordered {
                x += draw::put(buf, x, area.y, g.h, area.right().saturating_sub(x), bstyle);
                x += draw::put(
                    buf,
                    x,
                    area.y,
                    g.title_l,
                    area.right().saturating_sub(x),
                    bstyle,
                );
            } else {
                x += draw::put(
                    buf,
                    x,
                    area.y,
                    " ",
                    area.right().saturating_sub(x),
                    ctx.s(Role::Selected),
                );
            }
            for (txt, st) in s.0 {
                x += draw::put(
                    buf,
                    x,
                    area.y,
                    &ctx.n(&txt),
                    area.right().saturating_sub(x),
                    st,
                );
            }
            if bordered {
                x += draw::put(
                    buf,
                    x,
                    area.y,
                    g.title_r,
                    area.right().saturating_sub(x),
                    bstyle,
                );
            }
        }
        text::blank_orphans(buf, area);
    }

    fn sem_style(&self, sem: Sem, ctx: &Ctx<'_>, revealed: bool) -> Style {
        match sem {
            Sem::Text(md) => md_style(md, ctx, revealed),
            Sem::Time => ctx.s(Role::Inactive),
            Sem::Nick(id) => ctx
                .base()
                .fg(ctx.theme.nick_color(id))
                .add_modifier(Modifier::BOLD),
            Sem::Gutter => ctx.s(Role::Divider),
            Sem::Dim => ctx.s(Role::Inactive),
            Sem::EmbedBar(c) => match c {
                Some(rgb) if rgb != 0 => ctx
                    .base()
                    .fg(ctx.theme.rgb(bbs_core::RgbColor::from_u32(rgb))),
                _ => ctx.s(Role::Divider),
            },
            Sem::EmbedTitle => ctx.s(Role::Title).add_modifier(Modifier::BOLD),
            Sem::FieldName => ctx.s(Role::Fg).add_modifier(Modifier::BOLD),
            Sem::Reaction(me) => {
                if me {
                    ctx.s(Role::Selected)
                } else {
                    ctx.s(Role::Fg)
                }
            }
            Sem::Tag => ctx.s(Role::Hotkey),
            Sem::Wave(played) => {
                if played {
                    ctx.s(Role::Hotkey)
                } else {
                    ctx.s(Role::Fg)
                }
            }
            Sem::Bar => ctx.base().fg(ctx.theme.gradient(GradientRole::Unread, 0.5)),
            Sem::Accent => ctx.s(Role::Hotkey),
        }
    }

    fn draw_messages(&mut self, l: &Layout, buf: &mut Buffer, ctx: &Ctx<'_>) {
        let area = l.messages;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let Some(ch) = self.active_channel else {
            draw_welcome(area, buf, ctx);
            return;
        };
        let Some((params, pkey, height)) = messages_view_params(self) else {
            return;
        };
        let scroll = self.scrolls.get(&ch).copied().unwrap_or_default();
        let store = &self.store;
        let Some(entry) = store.messages.get(&ch) else {
            draw::put(
                buf,
                area.x + 1,
                area.y,
                &ctx.n("loading…"),
                area.width,
                ctx.s(Role::Inactive),
            );
            return;
        };
        if entry.messages.is_empty() {
            let s = if entry.loading {
                "loading…"
            } else {
                "No messages yet. Press i to say hello."
            };
            draw::put(
                buf,
                area.x + 1,
                area.y + area.height.saturating_sub(1),
                &ctx.n(s),
                area.width,
                ctx.s(Role::Inactive),
            );
            return;
        }
        let res = |k, i| resolve_mention(store, k, i);
        let ment = |m: &bbs_core::Message| store.mentions_me(m);
        let tl = Timeline {
            msgs: &entry.messages,
            params,
            pkey,
            res: &res,
            mentions: &ment,
            playback: store.playback.as_ref(),
        };
        let view = tl.view(&mut self.cache, &scroll, height);
        let at_top = view.at_top;
        // Bottom-align when content is shorter than the viewport.
        let pad = height.saturating_sub(u16::try_from(view.lines.len()).unwrap_or(height));
        let text_x = if params.measure.gutter > 0 {
            params.measure.gutter
        } else {
            params.measure.indent
        };
        let t = params.measure.text;
        let sidecar_x = area.x + text_x + t + 1;
        let mut slots: Vec<ImageSlot> = Vec::new();
        let selected = self.selected;
        for (i, vl) in view.lines.iter().enumerate() {
            let y = area.y + pad + u16::try_from(i).unwrap_or(0);
            let is_sel = selected == Some(vl.message_id);
            let revealed = self.revealed.contains(&vl.message_id);
            let row_style = if is_sel {
                ctx.s(Role::Selected)
            } else {
                ctx.base()
            };
            if is_sel {
                draw::fill(
                    buf,
                    Rect::new(area.x, y, area.width.saturating_sub(1), 1),
                    row_style,
                );
            }
            let mut x = area.x;
            let maxx = area.right().saturating_sub(1);
            for Seg { text: s, sem } in &vl.line.segs {
                let mut st = self.sem_style(*sem, ctx, revealed);
                if vl.line.mention && matches!(sem, Sem::Time) {
                    st = ctx.s(Role::Mention).add_modifier(Modifier::BOLD);
                }
                if is_sel {
                    st = st.bg(ctx
                        .theme
                        .style(Role::Selected)
                        .bg
                        .unwrap_or(ratatui::style::Color::Reset));
                }
                let shown = if matches!(sem, Sem::Text(md) if md.spoiler) && !revealed {
                    let block = if ctx.g.is_ascii() { "#" } else { "▒" };
                    block.repeat(str_width(s))
                } else {
                    s.clone()
                };
                x += draw::put(buf, x, y, &shown, maxx.saturating_sub(x), st);
            }
            if vl.line.mention
                && !is_sel
                && params.measure.gutter == 0
                && buf[(area.x, y)].symbol() == " "
            {
                draw::cell(buf, area.x, y, ctx.g.embed_bar, ctx.s(Role::Mention));
            }
            if let Some(img) = &vl.line.image {
                let x0 = if img.sidecar {
                    sidecar_x
                } else {
                    area.x + text_x + img.x_off
                };
                let w = img.cols.min(maxx.saturating_sub(x0));
                if w > 0 {
                    // Collect/extend the slot for this image.
                    let box_top = y.saturating_sub(img.row.min(y - area.y));
                    let clip_top = img.row.saturating_sub(y - box_top);
                    if let Some(s) = slots
                        .iter_mut()
                        .find(|s| s.url == img.url && s.rect.x == x0)
                    {
                        s.visible.height = (y + 1).saturating_sub(s.visible.y);
                    } else {
                        let full_y = i32::from(y) - i32::from(img.row);
                        slots.push(ImageSlot {
                            url: img.url.clone(),
                            rect: Rect::new(
                                x0,
                                u16::try_from(full_y.max(0)).unwrap_or(0),
                                w,
                                img.rows,
                            ),
                            visible: Rect::new(x0, y, w, 1),
                            clip_top: img.row,
                            animated: img.animated,
                            focused: is_sel,
                        });
                        let _ = clip_top;
                    }
                }
            }
        }
        // Draw image boxes (placeholder / halfblock) over their visible rects.
        for s in &slots {
            let frame = self.images.get(&s.url);
            let label = view
                .lines
                .iter()
                .find_map(|vl| {
                    vl.line
                        .image
                        .as_ref()
                        .filter(|im| im.url == s.url)
                        .map(|im| im.label.clone())
                })
                .unwrap_or_default();
            if s.clip_top == 0 && s.visible.height == s.rect.height {
                crate::panes::render_image_box(self, buf, ctx, s.visible, &s.url, &label, frame);
            } else {
                // Partially visible: placeholder only (scroll-proxy semantics; runtime re-sends when settled).
                crate::panes::render_image_box(self, buf, ctx, s.visible, &s.url, &label, None);
            }
        }
        self.image_slots.extend(slots);
        draw::scrollbar(
            buf,
            ctx,
            area.right() - 1,
            area.y,
            area.height,
            view.above,
            view.lines.len(),
            view.below,
        );
        if at_top {
            self.request_older();
        }
    }

    fn draw_banners(&self, l: &Layout, buf: &mut Buffer, ctx: &Ctx<'_>) {
        let mut rows = l.banners.iter();
        if l.header.is_none()
            && let CallRingState::IncomingRinging {
                caller_name,
                is_group_dm,
                ..
            } = &self.store.ring
            && let Some(r) = rows.next()
        {
            let s = format!(
                "{} {} is calling{} - [a]nswer [h]ang up",
                ctx.g.phone,
                sanitize_single_line(caller_name),
                if *is_group_dm { " (group)" } else { "" }
            );
            let st = ctx
                .s(Role::Mention)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED);
            draw::fill(buf, *r, st);
            draw::put(buf, r.x, r.y, &ctx.n(&s), r.width, st);
        }
        if let Some(vc) = &self.store.voice
            && let bbs_core::DaveSessionState::Degraded { reason } = &vc.dave
            && let Some(r) = rows.next()
        {
            let s = format!("! DAVE! E2EE degraded: {}", sanitize_single_line(reason));
            let st = ctx
                .s(Role::Mention)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED);
            draw::fill(buf, *r, st);
            draw::put(buf, r.x, r.y, &ctx.n(&s), r.width, st);
        }
        if !self.composer.attachments.is_empty()
            && let Some(r) = rows.next()
        {
            let names: Vec<String> = self
                .composer
                .attachments
                .iter()
                .map(|p| {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            let s = format!("{} {}: {}", ctx.g.clip, names.len(), names.join(", "));
            draw::put(buf, r.x, r.y, &ctx.n(&s), r.width, ctx.s(Role::Hotkey));
        }
    }

    fn draw_separator(&self, l: &Layout, buf: &mut Buffer, ctx: &Ctx<'_>) {
        let Some(r) = l.separator else { return };
        let bordered = l.top_border.is_some();
        let rows = l.area.height;
        let st = if bordered {
            draw::border_style(ctx, Role::Divider, r.y - l.area.y, rows)
        } else {
            ctx.s(Role::Divider)
        };
        for x in r.left()..r.right() {
            draw::cell(buf, x, r.y, ctx.g.rule, st);
        }
        if bordered {
            draw::cell(buf, r.x.saturating_sub(1), r.y, ctx.g.sep_l, st);
            draw::cell(buf, r.right(), r.y, ctx.g.sep_r, st);
        }
        // Embedded reply target + typing indicator (0 extra rows).
        let mut parts: Vec<(String, Style)> = Vec::new();
        match &self.composer.target {
            ComposeTarget::Reply {
                author, snippet, ..
            } => {
                parts.push((
                    format!(
                        "{} {}: {}",
                        ctx.g.reply,
                        sanitize_single_line(author),
                        snippet
                    ),
                    ctx.s(Role::Hotkey),
                ));
            }
            ComposeTarget::Edit { .. } => {
                parts.push(("editing (Esc keeps draft)".to_string(), ctx.s(Role::Hotkey)))
            }
            ComposeTarget::New => {}
        }
        if let Some(ch) = self.active_channel {
            let names = self.store.typing_names(ch, self.now_unix());
            if !names.is_empty() {
                let who = if names.len() > 3 {
                    "several people".to_string()
                } else {
                    names
                        .iter()
                        .map(|n| sanitize_single_line(n).into_owned())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                parts.push((format!("{who} typing…"), ctx.s(Role::Inactive)));
            }
        }
        let mut x = r.x + 1;
        for (s, stl) in parts {
            if x + 4 >= r.right() {
                break;
            }
            x += draw::put(buf, x, r.y, ctx.g.title_l, r.right().saturating_sub(x), st);
            x += draw::put(
                buf,
                x,
                r.y,
                &ctx.n(&s),
                r.right().saturating_sub(x + 1),
                stl,
            );
            x += draw::put(buf, x, r.y, ctx.g.title_r, r.right().saturating_sub(x), st);
            x += 1;
        }
        text::blank_orphans(buf, r);
    }

    fn draw_composer(&mut self, l: &Layout, buf: &mut Buffer, ctx: &Ctx<'_>) -> Option<(u16, u16)> {
        let r = l.composer;
        if r.width == 0 || r.height == 0 {
            return None;
        }
        let insert = self.mode == Mode::Insert && self.modals.is_empty();
        // Short chrome: reply prefix inline.
        let prefix = if l.separator.is_none() {
            match &self.composer.target {
                ComposeTarget::Reply { author, .. } => format!(
                    "{}{}>",
                    ctx.g.reply,
                    text::truncate(&sanitize_single_line(author), 8, ctx.g.ellipsis)
                ),
                ComposeTarget::Edit { .. } => "edit>".to_string(),
                ComposeTarget::New => ">".to_string(),
            }
        } else {
            ">".to_string()
        };
        let prefix = ctx.n(&prefix);
        let pw = u16::try_from(str_width(&prefix))
            .unwrap_or(1)
            .min(r.width.saturating_sub(2));
        draw::put(
            buf,
            r.x,
            r.y,
            &prefix,
            pw,
            if insert {
                ctx.s(Role::Hotkey)
            } else {
                ctx.s(Role::Inactive)
            },
        );
        let tw = r.width.saturating_sub(pw + 1).max(2);
        let tx = r.x + pw + 1;
        if self.composer.is_empty() {
            let ch_name = self
                .active_channel
                .and_then(|c| self.store.channel(c))
                .map(|c| sanitize_single_line(&c.display_label()).into_owned())
                .unwrap_or_default();
            let hint = if insert {
                format!("Message {ch_name} - Enter send, Ctrl+J newline, Esc normal")
            } else {
                "[i] compose  [k] jump  [?] help".to_string()
            };
            draw::put(buf, tx, r.y, &ctx.n(&hint), tw, ctx.s(Role::Inactive));
            return insert.then_some((tx, r.y));
        }
        let (lines, crow, ccol) = self.composer.layout(tw);
        let h = r.height;
        let first = crow.saturating_sub(h.saturating_sub(1));
        for (i, line) in lines
            .iter()
            .enumerate()
            .skip(usize::from(first))
            .take(usize::from(h))
        {
            let y = r.y + u16::try_from(i).unwrap_or(0) - first;
            draw::put(buf, tx, y, &ctx.n(line), tw, ctx.s(Role::Fg));
        }
        // Length counter near the limit.
        let len = self.composer.char_len();
        let max = self.cfg.max_message_len();
        if len * 10 >= max * 9 {
            let s = format!("{len}/{max}");
            let st = if len > max {
                ctx.s(Role::Mention).add_modifier(Modifier::BOLD)
            } else {
                ctx.s(Role::Inactive)
            };
            draw::put_right(buf, r.x, r.bottom() - 1, &s, r.width, st);
        }
        insert.then_some((tx + ccol.min(tw.saturating_sub(1)), r.y + crow - first))
    }
}

fn md_style(md: MdStyle, ctx: &Ctx<'_>, _revealed: bool) -> Style {
    let mut st = if md.code {
        ctx.s(Role::Hotkey)
    } else if md.mention {
        ctx.s(Role::Mention).add_modifier(Modifier::BOLD)
    } else if md.link {
        ctx.s(Role::Hotkey).add_modifier(Modifier::UNDERLINED)
    } else if md.subtext {
        ctx.s(Role::Inactive)
    } else {
        ctx.s(Role::Fg)
    };
    if let Some(fg) = md.fg {
        st = st.fg(ctx.theme.rgb(fg));
    }
    if md.bold {
        st = st.add_modifier(Modifier::BOLD);
    }
    if md.italic {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if md.underline {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    if md.strike {
        st = st.add_modifier(Modifier::CROSSED_OUT);
    }
    st
}

fn offset_layout(l: &mut Layout, dx: u16, dy: u16) {
    if dx == 0 && dy == 0 {
        return;
    }
    let mv = |r: &mut Rect| {
        r.x += dx;
        r.y += dy;
    };
    mv(&mut l.area);
    for p in &mut l.panes {
        mv(&mut p.rect);
    }
    for d in &mut l.dividers {
        *d += dx;
    }
    for r in [
        &mut l.header,
        &mut l.top_border,
        &mut l.bottom_border,
        &mut l.separator,
    ]
    .into_iter()
    .flatten()
    {
        mv(r);
    }
    mv(&mut l.messages);
    mv(&mut l.composer);
    for b in &mut l.banners {
        mv(b);
    }
}

/// BBS welcome screen shown before any channel is open.
pub fn draw_welcome(area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    let art_rich = [
        "╔╗ ╔╗ ╔═╗╔═╗╔═╗╦═╗╔╦╗",
        "╠╩╗╠╩╗╚═╗║  ║ ║╠╦╝ ║║",
        "╚═╝╚═╝╚═╝╚═╝╚═╝╩╚══╩╝",
    ];
    let art_ascii = [
        "+-+ +-+ +-+ +-+ +-+ +-+ +-+",
        "|B| |B| |S| |C| |O| |R| |D|",
        "+-+ +-+ +-+ +-+ +-+ +-+ +-+",
    ];
    let art: &[&str] = if ctx.g.is_ascii() {
        &art_ascii
    } else {
        &art_rich
    };
    let h = u16::try_from(art.len()).unwrap_or(0) + 2;
    let y0 = area.y + area.height.saturating_sub(h) / 2;
    for (i, line) in art.iter().enumerate() {
        let w = u16::try_from(str_width(line)).unwrap_or(0);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let t = if art.len() <= 1 {
            0.0
        } else {
            i as f32 / (art.len() - 1) as f32
        };
        draw::put(
            buf,
            x,
            y0 + u16::try_from(i).unwrap_or(0),
            line,
            area.width,
            ctx.base().fg(ctx.theme.gradient(GradientRole::Border, t)),
        );
    }
    let tag = ctx.n("press k to jump to a channel · ? for help");
    let w = u16::try_from(str_width(&tag)).unwrap_or(0);
    draw::put(
        buf,
        area.x + area.width.saturating_sub(w) / 2,
        y0 + h - 1,
        &tag,
        area.width,
        ctx.s(Role::Inactive),
    );
}
