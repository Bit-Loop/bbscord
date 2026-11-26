//! Side panes: guild/channel tree, members, Ultra decks (Stage, Media inspector, Thread/Pins,
//! Mentions inbox) and the shared image-box renderer (placeholder or HalfBlock, §6.2).

use bbs_core::{
    ChannelKind, DaveSessionState, GraphicsTier, PresenceStatus, RgbaFrame, Snowflake,
    VideoSinkKind, VoiceConnectionState,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::app::{App, Focus, SideItem};
use crate::draw::{self, Ctx};
use crate::meters;
use crate::sanitize::sanitize_single_line;
use crate::text::{self, str_width};
use crate::theme::{GradientRole, Role};

/// Content kind of an Ultra deck slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckKind {
    /// Voice / DAVE / screenshare stage.
    Stage,
    /// Media / GIF inspector.
    Media,
    /// Thread & pins.
    Threads,
    /// Mentions inbox.
    Mentions,
}

/// Deck content for slot `i` (§2.2 priority lists).
#[must_use]
pub fn deck_kind(app: &App, i: u8) -> DeckKind {
    let in_call =
        app.store.voice.is_some() || !matches!(app.store.ring, bbs_core::CallRingState::Idle);
    let order = if in_call {
        [DeckKind::Stage, DeckKind::Media, DeckKind::Threads]
    } else {
        [DeckKind::Threads, DeckKind::Media, DeckKind::Mentions]
    };
    order[usize::from(i.min(2))]
}

fn sigil(ctx: &Ctx<'_>, k: ChannelKind) -> &'static str {
    if ctx.g.is_ascii() || ctx.cfg.charset == bbs_core::ResolvedCharset::VtSafe {
        k.ascii_sigil()
    } else {
        k.utf8_sigil()
    }
}

/// Guild / channel tree.
pub fn render_sidebar(app: &mut App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let items = app.sidebar_items();
    if items.is_empty() {
        return;
    }
    app.sidebar.cursor = app.sidebar.cursor.min(items.len() - 1);
    let h = usize::from(area.height);
    if app.sidebar.cursor < app.sidebar.offset {
        app.sidebar.offset = app.sidebar.cursor;
    } else if app.sidebar.cursor >= app.sidebar.offset + h {
        app.sidebar.offset = app.sidebar.cursor + 1 - h;
    }
    let focused = app.focus == Focus::Sidebar;
    for (row, item) in items.iter().enumerate().skip(app.sidebar.offset).take(h) {
        let y = area.y + u16::try_from(row - app.sidebar.offset).unwrap_or(0);
        let (label, badge, style) = match item {
            SideItem::Home => {
                let (u, m) = app.store.dm_unread();
                let open = app.sidebar.expanded.contains(&None);
                let mark = if open {
                    ctx.g.expanded
                } else {
                    ctx.g.collapsed
                };
                (
                    format!("{mark}DMs"),
                    badge(m, u),
                    ctx.s(Role::Title).add_modifier(Modifier::BOLD),
                )
            }
            SideItem::Guild(id) => {
                let g = app.store.guild(*id);
                let name = g
                    .map(|g| sanitize_single_line(&g.name).into_owned())
                    .unwrap_or_default();
                let open = app.sidebar.expanded.contains(&Some(*id));
                let mark = if open {
                    ctx.g.expanded
                } else {
                    ctx.g.collapsed
                };
                let (u, m) = g.map_or((0, 0), |g| (u32::from(g.unread), g.mention_count));
                let st = if u > 0 || m > 0 {
                    ctx.s(Role::Fg).add_modifier(Modifier::BOLD)
                } else {
                    ctx.s(Role::Fg)
                };
                (format!("{mark}{name}"), badge(m, u), st)
            }
            SideItem::Channel(id, depth) => {
                let Some(c) = app.store.channel(*id) else {
                    continue;
                };
                let indent = " ".repeat(usize::from(*depth));
                let name = if c.kind == ChannelKind::GuildCategory {
                    let open = !app.sidebar.collapsed.contains(&c.id);
                    format!(
                        "{}{}",
                        if open {
                            ctx.g.expanded
                        } else {
                            ctx.g.collapsed
                        },
                        sanitize_single_line(&c.name).to_uppercase()
                    )
                } else {
                    format!(
                        "{}{}",
                        sigil(ctx, c.kind),
                        sanitize_single_line(&c.display_label())
                    )
                };
                let active = app.active_channel == Some(c.id);
                let unread = c.unread_count > 0 || c.mention_count > 0;
                let st = if active {
                    ctx.s(Role::Hotkey).add_modifier(Modifier::BOLD)
                } else if c.kind == ChannelKind::GuildCategory {
                    ctx.s(Role::Inactive).add_modifier(Modifier::BOLD)
                } else if unread {
                    ctx.s(Role::Fg).add_modifier(Modifier::BOLD)
                } else {
                    ctx.s(Role::Inactive)
                };
                let in_voice = app
                    .store
                    .voice
                    .as_ref()
                    .is_some_and(|v| v.channel_id == c.id);
                let b = if in_voice {
                    ctx.g.speaking.to_string()
                } else {
                    badge(c.mention_count, c.unread_count)
                };
                (format!("{indent}{name}"), b, st)
            }
        };
        let selected = focused && row == app.sidebar.cursor;
        let bw = u16::try_from(str_width(&badge)).unwrap_or(0);
        let lw = area.width.saturating_sub(if bw > 0 { bw + 1 } else { 0 });
        draw::list_row(buf, ctx, area.x, y, lw, &ctx.n(&label), selected, style);
        if bw > 0 && area.width > bw {
            let st = if badge.starts_with('@') {
                ctx.s(Role::Mention).add_modifier(Modifier::BOLD)
            } else {
                ctx.s(Role::Hotkey)
            };
            draw::put_right(buf, area.x + lw, y, &badge, area.width - lw, st);
        }
    }
}

fn badge(mentions: u32, unread: u32) -> String {
    if mentions > 0 {
        format!("@{}", mentions.min(99))
    } else if unread > 0 {
        format!("{}", unread.min(99))
    } else {
        String::new()
    }
}

/// Members list (authors seen + DM recipients + voice participants).
pub fn render_members(app: &App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>, overlay: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut ids: Vec<Snowflake> = Vec::new();
    if let Some(ch) = app.active_channel {
        if let Some(c) = app.store.channel(ch) {
            ids.extend(c.recipients.iter().map(|u| u.id));
        }
        if let Some(ms) = app.store.messages.get(&ch) {
            for m in ms.messages.iter().rev() {
                if !ids.contains(&m.author.id) {
                    ids.push(m.author.id);
                }
            }
        }
    }
    let mut members: Vec<(PresenceStatus, String, Snowflake)> = ids
        .iter()
        .filter_map(|id| app.store.users.get(id))
        .map(|u| {
            (
                app.store.presence.get(&u.id).copied().unwrap_or(u.presence),
                sanitize_single_line(u.display_name()).into_owned(),
                u.id,
            )
        })
        .collect();
    let rank = |p: PresenceStatus| match p {
        PresenceStatus::Online => 0,
        PresenceStatus::Idle => 1,
        PresenceStatus::DoNotDisturb => 2,
        _ => 3,
    };
    members.sort_by(|a, b| {
        rank(a.0)
            .cmp(&rank(b.0))
            .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    let h = usize::from(area.height);
    let focused = overlay || app.focus == Focus::Members;
    let cursor = app.members_cursor.min(members.len().saturating_sub(1));
    let start = cursor.saturating_sub(h.saturating_sub(1));
    for (i, (p, name, id)) in members.iter().enumerate().skip(start).take(h) {
        let y = area.y + u16::try_from(i - start).unwrap_or(0);
        let sym = if ctx.g.is_ascii() || ctx.cfg.charset == bbs_core::ResolvedCharset::VtSafe {
            p.ascii_symbol()
        } else {
            p.utf8_symbol()
        };
        let st = match p {
            PresenceStatus::Online => ctx.base().fg(ctx.theme.nick_color(id.as_u64())),
            PresenceStatus::Offline | PresenceStatus::Invisible => ctx.s(Role::Inactive),
            _ => ctx.s(Role::Fg),
        };
        draw::list_row(
            buf,
            ctx,
            area.x,
            y,
            area.width,
            &ctx.n(&format!("{sym} {name}")),
            focused && i == cursor,
            st,
        );
    }
}

/// Stage deck: voice header, streams/video, participants with Braille VU, DAVE line.
pub fn render_stage(app: &mut App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>, overlay: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(v) = app.store.voice.clone() else {
        let lines = [
            "Not in voice.".to_string(),
            "[v] join selected voice channel".to_string(),
            "[a] call this DM / Group DM".to_string(),
        ];
        for (i, l) in lines.iter().enumerate() {
            draw::put(
                buf,
                area.x,
                area.y + u16::try_from(i).unwrap_or(0),
                &ctx.n(l),
                area.width,
                ctx.s(Role::Inactive),
            );
        }
        return;
    };
    let mut y = area.y;
    let bottom = area.bottom();
    let state = match v.connection_state {
        VoiceConnectionState::Connected => "",
        VoiceConnectionState::Connecting => " connecting…",
        VoiceConnectionState::Signaling => " signaling…",
        VoiceConnectionState::Reconnecting => " reconnecting…",
        VoiceConnectionState::Disconnected => " disconnected",
    };
    let all_verified = app.is_dave_all_verified();
    let (dave_txt, dave_role) = dave_badge(ctx, &v.dave, all_verified);
    let ping = v
        .ping_ms
        .map(|p| format!(" {} {p}ms", ctx.g.dot))
        .unwrap_or_default();
    let head_prefix = format!(
        "{} {} {} {}",
        ctx.g.speaking,
        sanitize_single_line(&v.channel_name),
        ctx.g.dot,
        v.participants.len(),
    );
    let head_tail = format!("{ping}{state}");
    let title_st = ctx.s(Role::Title).add_modifier(Modifier::BOLD);
    let mut hx = area.x;
    hx += draw::put(
        buf,
        hx,
        y,
        &ctx.n(&head_prefix),
        area.right().saturating_sub(hx),
        title_st,
    );
    if !dave_txt.is_empty() {
        hx += draw::put(
            buf,
            hx,
            y,
            &ctx.n(&dave_txt),
            area.right().saturating_sub(hx),
            ctx.s(dave_role).add_modifier(Modifier::BOLD),
        );
    }
    if !head_tail.is_empty() {
        draw::put(
            buf,
            hx,
            y,
            &ctx.n(&head_tail),
            area.right().saturating_sub(hx),
            title_st,
        );
    }
    y += 1;
    let flags = format!(
        "{}{}{}",
        if v.self_mute { "[M]uted " } else { "" },
        if v.self_deaf { "[D]eaf " } else { "" },
        if app.broadcasting { "[S]haring " } else { "" }
    );
    if !flags.is_empty() && y < bottom {
        draw::put(
            buf,
            area.x,
            y,
            &ctx.n(&flags),
            area.width,
            ctx.s(Role::Mention),
        );
        y += 1;
    }
    if let DaveSessionState::Degraded { reason } = &v.dave
        && y < bottom
    {
        let s = format!("! DAVE downgrade: {}", sanitize_single_line(reason));
        draw::put(
            buf,
            area.x,
            y,
            &ctx.n(&s),
            area.width,
            ctx.s(Role::Mention)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        );
        y += 1;
    }

    // Streams + inline video (gate §6.5).
    let streams = app
        .store
        .streams
        .get(&v.channel_id)
        .cloned()
        .unwrap_or_default();
    let deck_rows = area.height;
    let inline_ok = VideoSinkKind::can_use_inline_terminal(
        area.width,
        deck_rows,
        ctx.cfg.graphics,
        ctx.cfg.over_ssh,
        ctx.cfg.inline_video_low_tier,
        ctx.cfg.inline_video_ssh,
    );
    let inline_active = app.cfg.prefer_inline_video && inline_ok;
    if let (Some(key), true) = (app.watching.clone(), inline_active) {
        let (fw, fh) = app
            .store
            .video_frame
            .as_ref()
            .filter(|f| f.stream_key == key)
            .map(|f| (f.width, f.height))
            .or_else(|| {
                streams
                    .iter()
                    .find(|s| s.stream_key == key)
                    .map(|s| s.resolution)
            })
            .unwrap_or((16, 9));
        if let Some((bw, bh)) =
            VideoSinkKind::inline_video_box(area.width, deck_rows, fw, fh, ctx.cfg.cell_px)
        {
            let bh = bh.min(bottom.saturating_sub(y).saturating_sub(
                VideoSinkKind::INLINE_RESERVED_STAGE_ROWS.min(bottom.saturating_sub(y)),
            ));
            if bh > 0 {
                let rect = Rect::new(area.x + 1, y, bw.min(area.width.saturating_sub(1)), bh);
                let frame = app
                    .store
                    .video_frame
                    .as_ref()
                    .filter(|f| f.stream_key == key)
                    .map(|f| RgbaFrame {
                        width: f.width,
                        height: f.height,
                        delay_ms: 0,
                        pixels: f.rgba.clone(),
                    });
                render_image_box(app, buf, ctx, rect, &key, "[stream]", frame.as_ref());
                let visible = rect.intersection(buf.area);
                if visible.width > 0 && visible.height > 0 {
                    app.image_slots.push(crate::app::ImageSlot {
                        url: key,
                        rect,
                        visible,
                        clip_top: visible.y.saturating_sub(rect.y),
                        animated: true,
                        focused: true,
                    });
                }
                y += bh;
            }
        }
    }
    for s in &streams {
        if y >= bottom {
            break;
        }
        let watching = app.watching.as_deref() == Some(s.stream_key.as_str());
        let line = format!(
            "{} {}: {} {}",
            ctx.g.play,
            sanitize_single_line(&s.owner_name),
            sanitize_single_line(&s.title),
            if watching { "[w] stop" } else { "[w]atch" }
        );
        draw::put(
            buf,
            area.x,
            y,
            &ctx.n(&line),
            area.width,
            ctx.s(Role::Hotkey),
        );
        y += 1;
    }

    // Participants.
    let focused = overlay || matches!(app.focus, Focus::Deck(_));
    let cursor = app.deck_cursor.min(v.participants.len().saturating_sub(1));
    let reserve = u16::from(area.height > 6);
    let rows_left = usize::from(bottom.saturating_sub(y).saturating_sub(reserve));
    let start = cursor.saturating_sub(rows_left.saturating_sub(1));
    for (i, p) in v
        .participants
        .iter()
        .enumerate()
        .skip(start)
        .take(rows_left)
    {
        let me = app.store.me.as_ref().is_some_and(|u| u.id == p.user_id);
        let dot = if p.speaking {
            ctx.g.speaking
        } else {
            ctx.g.silent
        };
        let vu_w: usize = if area.width >= 48 {
            8
        } else if area.width >= 32 {
            5
        } else {
            0
        };
        let name_w = usize::from(area.width)
            .saturating_sub(2 + vu_w + 1 + 5 + 5 + 4)
            .clamp(3, 20);
        let name = text::fit(&ctx.n(&sanitize_single_line(&p.display_name)), name_w);
        let verified = p.dave_verified || app.dave_verified.contains(&p.user_id);
        let marks = format!(
            "{}{}{}",
            if p.self_mute || p.server_mute {
                "M"
            } else {
                " "
            },
            if p.self_deaf || p.server_deaf {
                "D"
            } else {
                " "
            },
            if verified { ctx.g.check } else { " " }
        );
        let ry = y + u16::try_from(i - start).unwrap_or(0);
        let selected = focused && i == cursor;
        let base = if selected {
            ctx.s(Role::Selected)
        } else {
            ctx.base()
        };
        draw::fill(buf, Rect::new(area.x, ry, area.width, 1), base);
        let mut x = area.x;
        let dot_style = if p.speaking {
            ctx.base().fg(ctx.theme.gradient(
                if me {
                    GradientRole::VuMic
                } else {
                    GradientRole::VuRemote
                },
                1.0,
            ))
        } else {
            ctx.s(Role::Inactive)
        };
        x += draw::put(buf, x, ry, dot, 1, if selected { base } else { dot_style });
        x += draw::put(buf, x, ry, " ", 1, base);
        x += draw::put(
            buf,
            x,
            ry,
            &name,
            u16::try_from(name_w).unwrap_or(0),
            if selected {
                base
            } else {
                ctx.base().fg(ctx.theme.nick_color(p.user_id.as_u64()))
            },
        );
        if vu_w > 0 {
            x += draw::put(buf, x, ry, " ", 1, base);
            let level = p.normalized_vu();
            let cells = meters::vu_cells(level, vu_w, ctx.cfg.charset);
            let lit = meters::vu_lit_cells(level, vu_w, ctx.cfg.charset);
            let grad = if me {
                GradientRole::VuMic
            } else {
                GradientRole::VuRemote
            };
            for (ci, c) in cells.iter().enumerate() {
                // Colour by the dB position of the cell (stops at −18/−6 dB).
                let t = meters::vu_gradient_t((ci as f32 + 1.0) / vu_w.max(1) as f32);
                let mut s = String::new();
                s.push(*c);
                let st = if ci >= lit {
                    ctx.s(Role::MeterBg)
                } else {
                    ctx.base().fg(ctx.theme.gradient(grad, t))
                };
                x += draw::put(buf, x, ry, &s, 1, st);
            }
        }
        let tail = format!(
            " {:>3}dB {:>3}% {marks}",
            p.rms_db.clamp(-60.0, 0.0).round() as i32,
            p.volume
        );
        draw::put(
            buf,
            x,
            ry,
            &ctx.n(&tail),
            area.right().saturating_sub(x),
            if selected {
                base
            } else {
                ctx.s(Role::Inactive)
            },
        );
    }
    if area.height > 6 {
        let hint = if overlay {
            "-/+ vol 0 reset x mute v verify w watch W sink s share h hang up"
        } else {
            "v verify  -/+ vol  w watch  W sink"
        };
        draw::put(
            buf,
            area.x,
            bottom - 1,
            &ctx.n(hint),
            area.width,
            ctx.s(Role::Inactive),
        );
    }
}

/// `(badge text, colour role)` for DAVE state (§10: `DAVE✓` only when all participants verified).
pub(crate) fn dave_badge(
    ctx: &Ctx<'_>,
    d: &DaveSessionState,
    all_verified: bool,
) -> (String, Role) {
    match d {
        DaveSessionState::Active { .. } if all_verified => {
            (format!(" DAVE{}", ctx.g.check), Role::Hotkey)
        }
        DaveSessionState::Active { .. } | DaveSessionState::Handshaking => {
            (format!(" DAVE{}", ctx.g.unverified), Role::Inactive)
        }
        DaveSessionState::Degraded { .. } => (" DAVE!".to_string(), Role::Mention),
        DaveSessionState::Disabled => (String::new(), Role::Inactive),
    }
}

/// Deck renderer.
pub fn render_deck(app: &mut App, i: u8, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    match deck_kind(app, i) {
        DeckKind::Stage => render_stage(app, area, buf, ctx, false),
        DeckKind::Media => render_media(app, area, buf, ctx),
        DeckKind::Threads => render_threads(app, area, buf, ctx),
        DeckKind::Mentions => render_mentions(app, area, buf, ctx),
    }
}

/// `(url, width, height, name, animated)`.
type MediaRef = (String, Option<u32>, Option<u32>, String, bool);

fn render_media(app: &mut App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    let msg = app
        .selected_message()
        .or_else(|| {
            app.active_messages().and_then(|ms| {
                ms.iter().rev().find(|m| {
                    m.attachments.iter().any(bbs_core::Attachment::is_image)
                        || m.embeds
                            .iter()
                            .any(|e| e.image.is_some() || e.thumbnail.is_some())
                })
            })
        })
        .cloned();
    let Some(m) = msg else {
        draw::put(
            buf,
            area.x,
            area.y,
            &ctx.n("No media. Select a message with an image."),
            area.width,
            ctx.s(Role::Inactive),
        );
        return;
    };
    let media: Option<MediaRef> = m
        .attachments
        .iter()
        .find(|a| a.is_image())
        .map(|a| {
            (
                a.proxy_url.clone().unwrap_or_else(|| a.url.clone()),
                a.width,
                a.height,
                sanitize_single_line(&a.filename).into_owned(),
                a.is_gif(),
            )
        })
        .or_else(|| {
            m.embeds.iter().find_map(|e| {
                e.image.as_ref().or(e.thumbnail.as_ref()).map(|im| {
                    (
                        im.proxy_url.clone().unwrap_or_else(|| im.url.clone()),
                        im.width,
                        im.height,
                        e.title.clone().unwrap_or_default(),
                        e.kind == bbs_core::EmbedKind::Gifv,
                    )
                })
            })
        });
    let Some((url, w, h, name, animated)) = media else {
        return;
    };
    let info = format!(
        "{} {}",
        sanitize_single_line(m.author.display_name()),
        sanitize_single_line(&name)
    );
    draw::put(
        buf,
        area.x,
        area.y,
        &ctx.n(&info),
        area.width,
        ctx.s(Role::Title),
    );
    let (bw, bh) = crate::messages::image_box(
        w,
        h,
        area.width,
        area.height.saturating_sub(2).max(1),
        ctx.cfg.cell_px,
    );
    let rect = Rect::new(
        area.x,
        area.y + 1,
        bw,
        bh.min(area.height.saturating_sub(1)),
    );
    let frame = app.images.get(&url).cloned();
    render_image_box(
        app,
        buf,
        ctx,
        rect,
        &url,
        &format!("[{}]", name),
        frame.as_ref(),
    );
    app.image_slots.push(crate::app::ImageSlot {
        url,
        rect,
        visible: rect,
        clip_top: 0,
        animated,
        focused: true,
    });
}

fn render_threads(app: &App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    let mut y = area.y;
    draw::put(
        buf,
        area.x,
        y,
        &ctx.n("Pinned"),
        area.width,
        ctx.s(Role::Title).add_modifier(Modifier::BOLD),
    );
    y += 1;
    if let Some(ms) = app.active_messages() {
        for m in ms.iter().filter(|m| m.pinned).rev() {
            if y >= area.bottom() {
                return;
            }
            let s = format!(
                "{}: {}",
                sanitize_single_line(m.author.display_name()),
                sanitize_single_line(&m.content)
            );
            draw::put(buf, area.x, y, &ctx.n(&s), area.width, ctx.s(Role::Fg));
            y += 1;
        }
    }
    y += 1;
    if y >= area.bottom() {
        return;
    }
    draw::put(
        buf,
        area.x,
        y,
        &ctx.n("Threads"),
        area.width,
        ctx.s(Role::Title).add_modifier(Modifier::BOLD),
    );
    y += 1;
    if let Some(ch) = app.active_channel {
        for chans in app.store.channels.values() {
            for t in chans
                .iter()
                .filter(|t| t.kind.is_thread() && t.parent_id == Some(ch))
            {
                if y >= area.bottom() {
                    return;
                }
                let s = format!("{}{}", sigil(ctx, t.kind), sanitize_single_line(&t.name));
                draw::put(buf, area.x, y, &ctx.n(&s), area.width, ctx.s(Role::Fg));
                y += 1;
            }
        }
    }
}

fn render_mentions(app: &App, area: Rect, buf: &mut Buffer, ctx: &Ctx<'_>) {
    let mut y = area.y;
    draw::put(
        buf,
        area.x,
        y,
        &ctx.n("Mentions"),
        area.width,
        ctx.s(Role::Title).add_modifier(Modifier::BOLD),
    );
    y += 1;
    let mut found: Vec<&bbs_core::Message> = app
        .store
        .messages
        .values()
        .flat_map(|c| c.messages.iter())
        .filter(|m| app.store.mentions_me(m))
        .collect();
    found.sort_by_key(|m| std::cmp::Reverse(m.id.as_u64()));
    for m in found {
        if y >= area.bottom() {
            break;
        }
        let ch = app
            .store
            .channel(m.channel_id)
            .map(|c| c.display_label())
            .unwrap_or_default();
        let s = format!(
            "{} {}: {}",
            sanitize_single_line(&ch),
            sanitize_single_line(m.author.display_name()),
            sanitize_single_line(&m.content)
        );
        draw::put(buf, area.x, y, &ctx.n(&s), area.width, ctx.s(Role::Fg));
        y += 1;
    }
}

/// Draw an image box: HalfBlock pixels when allowed and decoded pixels exist, otherwise a
/// placeholder with a centred label (the graphics runtime overlays real images on top).
pub fn render_image_box(
    app: &App,
    buf: &mut Buffer,
    ctx: &Ctx<'_>,
    rect: Rect,
    url: &str,
    label: &str,
    frame: Option<&RgbaFrame>,
) {
    let rect = rect.intersection(buf.area);
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let frame = frame.or_else(|| app.images.get(url));
    let halfblock = matches!(ctx.cfg.graphics, GraphicsTier::HalfBlock)
        && matches!(
            ctx.cfg.depth,
            bbs_core::ColorDepth::TrueColor | bbs_core::ColorDepth::Ansi256
        )
        && ctx.cfg.charset != bbs_core::ResolvedCharset::Ascii7Bit;
    if let (true, Some(f)) = (halfblock, frame) {
        halfblock_frame(buf, ctx, rect, f);
        return;
    }
    let shade = if ctx.g.is_ascii() { "." } else { "░" };
    let st = ctx.s(Role::Inactive);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            draw::cell(buf, x, y, shade, st);
        }
    }
    let lbl = text::truncate(&ctx.n(label), usize::from(rect.width), ctx.g.ellipsis);
    let lw = u16::try_from(str_width(&lbl)).unwrap_or(0);
    let x = rect.x + (rect.width.saturating_sub(lw)) / 2;
    let y = rect.y + rect.height / 2;
    draw::put(buf, x, y, &lbl, rect.width, ctx.s(Role::Fg));
}

/// HalfBlock rasteriser: `▀` with fg = top pixel, bg = bottom pixel (nearest sampling).
pub fn halfblock_frame(buf: &mut Buffer, ctx: &Ctx<'_>, rect: Rect, f: &RgbaFrame) {
    let (w, h) = (f.width as usize, f.height as usize);
    if w == 0 || h == 0 || f.pixels.len() < w * h * 4 {
        return;
    }
    let cols = usize::from(rect.width);
    let rows = usize::from(rect.height) * 2;
    let px = |x: usize, y: usize| -> Color {
        let sx = (x * w / cols.max(1)).min(w - 1);
        let sy = (y * h / rows.max(1)).min(h - 1);
        let i = (sy * w + sx) * 4;
        let c = bbs_core::RgbColor::new(f.pixels[i], f.pixels[i + 1], f.pixels[i + 2]);
        ctx.theme.rgb(c)
    };
    for cy in 0..usize::from(rect.height) {
        for cx in 0..cols {
            let top = px(cx, cy * 2);
            let bot = px(cx, cy * 2 + 1);
            let x = rect.x + u16::try_from(cx).unwrap_or(0);
            let y = rect.y + u16::try_from(cy).unwrap_or(0);
            draw::cell(buf, x, y, "▀", Style::default().fg(top).bg(bot));
        }
    }
}
