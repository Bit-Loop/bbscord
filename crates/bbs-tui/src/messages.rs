//! Message layout engine (§2.3, §4): turns a [`Message`] into anchor-tagged visual lines.
//!
//! Lines carry *semantic* styles ([`Sem`]) so a theme switch never invalidates the layout cache;
//! colours are resolved at draw time.

use bbs_core::{AnchorPart, Embed, EmbedKind, Message, Snowflake};

use crate::charset::Glyphs;
use crate::geometry::ChatMeasure;
use crate::markdown::{self, MdLine, MdSpan, MdStyle, MentionKind};
use crate::sanitize::{sanitize_remote, sanitize_single_line};
use crate::text::{self, TextCtx, str_width};

/// Semantic style of a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sem {
    /// Markdown text.
    Text(MdStyle),
    /// Timestamp.
    Time,
    /// Author nick (user id for colour hashing).
    Nick(u64),
    /// Gutter separator / blank gutter.
    Gutter,
    /// Dim informational text.
    Dim,
    /// Embed colour bar (embed RGB).
    EmbedBar(Option<u32>),
    /// Embed title.
    EmbedTitle,
    /// Embed field name.
    FieldName,
    /// Reaction chip (`me`).
    Reaction(bool),
    /// Attachment / media tag.
    Tag,
    /// Waveform cells (played?).
    Wave(bool),
    /// Poll / progress bar.
    Bar,
    /// Hotkey-coloured accent.
    Accent,
}

/// Styled run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    /// Text.
    pub text: String,
    /// Semantic style.
    pub sem: Sem,
}

impl Seg {
    fn new(text: impl Into<String>, sem: Sem) -> Self {
        Self {
            text: text.into(),
            sem,
        }
    }
}

/// One row of an image box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRow {
    /// Media URL (cache key for the graphics runtime).
    pub url: String,
    /// Row within the box.
    pub row: u16,
    /// Box rows.
    pub rows: u16,
    /// Box columns.
    pub cols: u16,
    /// Column offset of the box from the text column start (or side-car start).
    pub x_off: u16,
    /// Placeholder label.
    pub label: String,
    /// Animated (GIF/GIFV).
    pub animated: bool,
    /// Drawn in the side-car column.
    pub sidecar: bool,
}

/// A visual line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VLine {
    /// Anchor part this line belongs to.
    pub part: AnchorPart,
    /// Segments, left to right, starting at the chat pane's left edge.
    pub segs: Vec<Seg>,
    /// Image row drawn on this line.
    pub image: Option<ImageRow>,
    /// Message mentions the user (gutter stripe).
    pub mention: bool,
}

impl VLine {
    fn new(part: AnchorPart, segs: Vec<Seg>) -> Self {
        Self {
            part,
            segs,
            image: None,
            mention: false,
        }
    }

    /// Display width of the segments.
    #[must_use]
    pub fn width(&self) -> usize {
        self.segs.iter().map(|s| str_width(&s.text)).sum()
    }
}

/// Parameters for laying out messages at one width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutParams {
    /// Chat pane inner width (scrollbar column excluded by the engine).
    pub width: u16,
    /// Measure.
    pub measure: ChatMeasure,
    /// Text context.
    pub ctx: TextCtx,
    /// Glyphs.
    pub glyphs: Glyphs,
    /// Max inline embed columns for the profile.
    pub max_inline_cols: u8,
    /// Cell pixel size.
    pub cell_px: (u16, u16),
    /// Max image rows.
    pub max_img_rows: u16,
    /// UTC offset in minutes for timestamps.
    pub utc_offset_min: i32,
    /// Text-tag-only graphics (1-row tags instead of boxes).
    pub text_tags: bool,
}

/// Name resolution for mentions.
pub trait Resolve {
    /// Resolve a mention.
    fn resolve(&self, kind: MentionKind, id: u64) -> Option<String>;
}

impl<F: Fn(MentionKind, u64) -> Option<String>> Resolve for F {
    fn resolve(&self, kind: MentionKind, id: u64) -> Option<String> {
        self(kind, id)
    }
}

/// `HH:MM` from a snowflake timestamp with a UTC offset.
#[must_use]
pub fn fmt_time(id: Snowflake, utc_offset_min: i32) -> String {
    let ms = i64::try_from(id.timestamp_ms()).unwrap_or(0);
    let mins = ms / 60_000 + i64::from(utc_offset_min);
    let day_min = mins.rem_euclid(1440);
    format!("{:02}:{:02}", day_min / 60, day_min % 60)
}

/// Human file size.
#[must_use]
pub fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    if bytes >= KB * KB {
        format!("{:.1} MB", bytes as f64 / (KB * KB) as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// `m:ss`.
#[must_use]
pub fn fmt_dur(secs: f32) -> String {
    let s = if secs.is_finite() {
        secs.max(0.0) as u64
    } else {
        0
    };
    format!("{}:{:02}", s / 60, s % 60)
}

/// Whether `cur` continues a group started by `prev` (same author within 7 min, no reply).
#[must_use]
pub fn is_grouped(prev: Option<&Message>, cur: &Message) -> bool {
    prev.is_some_and(|p| {
        p.author.id == cur.author.id
            && cur.reply_to.is_none()
            && cur.id.timestamp_ms().saturating_sub(p.id.timestamp_ms()) <= 7 * 60_000
    })
}

/// Image box size in cells for pixel dims within limits (§4.4); all dims ≥ 1.
#[must_use]
pub fn image_box(
    px_w: Option<u32>,
    px_h: Option<u32>,
    max_w: u16,
    max_h: u16,
    cell: (u16, u16),
) -> (u16, u16) {
    let (pw, ph) = match (px_w, px_h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (f64::from(w), f64::from(h)),
        _ => (4.0, 3.0),
    };
    let cw = f64::from(cell.0.max(1));
    let ch = f64::from(cell.1.max(1));
    let max_w = f64::from(max_w.max(1));
    let max_h = f64::from(max_h.max(1));
    // Natural size in cells (never upscale beyond native pixels).
    let nat_w = (pw / cw).max(1.0);
    let nat_h = (ph / ch).max(1.0);
    let scale = (max_w / nat_w).min(max_h / nat_h).min(1.0);
    let w = (nat_w * scale).round().clamp(1.0, max_w);
    let h = (nat_h * scale).round().clamp(1.0, max_h);
    (w as u16, h as u16)
}

struct Builder<'a> {
    p: LayoutParams,
    lines: Vec<VLine>,
    /// Column where body text starts (gutter or indent).
    text_x: u16,
    /// Text measure.
    t: u16,
    res: &'a dyn Resolve,
}

impl Builder<'_> {
    fn pad(&self) -> Seg {
        Seg::new(" ".repeat(usize::from(self.text_x)), Sem::Gutter)
    }

    fn push(&mut self, part: AnchorPart, mut segs: Vec<Seg>) {
        segs.insert(0, self.pad());
        self.lines.push(VLine::new(part, segs));
    }

    fn md(&self, src: &str) -> Vec<MdLine> {
        let clean = sanitize_remote(src);
        let res = self.res;
        markdown::render_markdown(&clean, &self.p.ctx, &|k, id| res.resolve(k, id))
    }

    fn norm1(&self, s: &str) -> String {
        text::normalize(&sanitize_single_line(s), &self.p.ctx).into_owned()
    }

    fn spans_to_segs(spans: &[MdSpan]) -> Vec<Seg> {
        spans
            .iter()
            .map(|s| Seg::new(s.text.clone(), Sem::Text(s.style)))
            .collect()
    }
}

/// Lay out one message. `grouped`: continuation of the previous author's group.
#[must_use]
pub fn layout_message(
    m: &Message,
    grouped: bool,
    mentions_me: bool,
    p: &LayoutParams,
    res: &dyn Resolve,
    playback: Option<&bbs_core::VoicePlaybackState>,
) -> Vec<VLine> {
    let g = p.glyphs;
    let usable = p.width.saturating_sub(1).max(1); // scrollbar column
    let gutter_mode = p.measure.gutter > 0;
    let text_x = if gutter_mode {
        p.measure.gutter
    } else {
        p.measure.indent
    }
    .min(usable.saturating_sub(1));
    let t = p.measure.text.min(usable.saturating_sub(text_x)).max(1);
    let mut b = Builder {
        p: *p,
        lines: Vec::new(),
        text_x,
        t,
        res,
    };

    let nick_raw = b.norm1(m.author.display_name());
    let time = fmt_time(m.id, p.utc_offset_min);

    // Reply context line.
    if let Some(r) = &m.reply_to {
        let who = r
            .author_name
            .as_deref()
            .map(|a| b.norm1(a))
            .unwrap_or_default();
        let snip = r.snippet.as_deref().map(|s| b.norm1(s)).unwrap_or_default();
        let s = format!("{} {who}: {snip}", g.reply);
        let s = text::truncate(&s, usize::from(t), g.ellipsis);
        b.push(AnchorPart::Header, vec![Seg::new(s, Sem::Dim)]);
    }

    // Header line in header mode.
    if !gutter_mode && !grouped {
        let nick = text::truncate(&nick_raw, usize::from(usable.saturating_sub(9)), g.ellipsis);
        let segs = vec![
            Seg::new(nick, Sem::Nick(m.author.id.as_u64())),
            Seg::new(format!(" {} {time}", g.dot), Sem::Time),
        ];
        b.lines.push(VLine::new(AnchorPart::Header, segs));
    }
    let body_start = b.lines.len();

    // Content.
    if !m.content.is_empty() {
        let md = b.md(&m.content);
        for line in markdown::wrap_lines(&md, usize::from(t)) {
            let mut segs = Builder::spans_to_segs(&line.spans);
            if matches!(line.kind, markdown::LineKind::Quote) {
                segs.insert(0, Seg::new(format!("{} ", g.thin_v), Sem::Dim));
            }
            b.push(
                AnchorPart::Content {
                    grapheme: line.start_grapheme,
                },
                segs,
            );
        }
        if m.edited_timestamp.is_some() {
            let suffix = " (edited)";
            if let Some(last) = b.lines.last_mut() {
                if last.width() + suffix.len() <= usize::from(usable) {
                    last.segs.push(Seg::new(suffix, Sem::Dim));
                } else {
                    let part = last.part;
                    b.push(part, vec![Seg::new(suffix.trim_start(), Sem::Dim)]);
                }
            }
        }
    }

    // Attachments.
    let sidecar = p.measure.sidecar;
    for (i, a) in m.attachments.iter().enumerate() {
        let idx = u16::try_from(i).unwrap_or(u16::MAX);
        let name = b.norm1(&a.filename);
        if let Some(vm) = &a.voice_message {
            let w = usize::from(t).saturating_sub(14).min(48);
            let pb = playback.filter(|pb| pb.message_id == m.id && pb.attachment_id == a.id);
            let total = pb.map_or(vm.duration_secs, |pb| {
                if pb.duration_secs > 0.0 {
                    pb.duration_secs
                } else {
                    vm.duration_secs
                }
            });
            let pos = pb.map_or(0.0, |pb| pb.position_secs.clamp(0.0, total.max(0.0)));
            let dur = format!("{}/{}", fmt_dur(pos), fmt_dur(total));
            let glyph = if pb.is_some_and(|pb| pb.playing) {
                g.pause
            } else {
                g.play
            };
            if w < 4 {
                b.push(
                    AnchorPart::Attachment {
                        index: idx,
                        permille: 0,
                    },
                    vec![Seg::new(format!("[{glyph} {dur}]"), Sem::Tag)],
                );
            } else {
                let wave = crate::meters::waveform(&vm.waveform, w, p.ctx.charset);
                let frac = if total > 0.0 {
                    (pos / total).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let played = ((w as f32) * frac).floor() as usize;
                let split = wave
                    .char_indices()
                    .nth(played)
                    .map_or(wave.len(), |(i, _)| i);
                b.push(
                    AnchorPart::Attachment {
                        index: idx,
                        permille: 0,
                    },
                    vec![
                        Seg::new(format!("{glyph} "), Sem::Accent),
                        Seg::new(wave[..split].to_string(), Sem::Wave(true)),
                        Seg::new(wave[split..].to_string(), Sem::Wave(false)),
                        Seg::new(format!(" {dur}"), Sem::Dim),
                    ],
                );
            }
        } else if a.is_image() && !p.text_tags {
            let (max_w, on_side) = if sidecar >= 12 {
                (sidecar, true)
            } else {
                (t.min(48), false)
            };
            let (bw, bh) = image_box(a.width, a.height, max_w, p.max_img_rows.max(1), p.cell_px);
            let dims = match (a.width, a.height) {
                (Some(w), Some(h)) => format!(" {w}x{h}"),
                _ => String::new(),
            };
            let label = format!(
                "[{}{} {name}{dims}]",
                if a.is_gif() { "GIF" } else { "IMG" },
                ""
            );
            push_image(
                &mut b,
                idx,
                a.proxy_url.clone().unwrap_or_else(|| a.url.clone()),
                bw,
                bh,
                label,
                a.is_gif(),
                on_side,
                body_start,
            );
        } else {
            let tag = if a.is_image() {
                let dims = match (a.width, a.height) {
                    (Some(w), Some(h)) => format!(" {w}x{h}"),
                    _ => String::new(),
                };
                format!(
                    "[{} {name}{dims} {}]",
                    if a.is_gif() { "GIF" } else { "IMG" },
                    fmt_size(a.size_bytes)
                )
            } else if a.is_video() {
                format!("[{} video {name} {}]", g.play, fmt_size(a.size_bytes))
            } else {
                format!("{} {name} {}", g.clip, fmt_size(a.size_bytes))
            };
            let tag = text::truncate(&text::normalize(&tag, &p.ctx), usize::from(t), g.ellipsis);
            b.push(
                AnchorPart::Attachment {
                    index: idx,
                    permille: 0,
                },
                vec![Seg::new(tag, Sem::Tag)],
            );
        }
    }

    // Embeds.
    if m.flags & Message::FLAG_SUPPRESS_EMBEDS == 0 {
        for (i, e) in m.embeds.iter().enumerate() {
            layout_embed(&mut b, e, u16::try_from(i).unwrap_or(u16::MAX), body_start);
        }
    }

    // Poll.
    if let Some(poll) = &m.poll {
        let q = b.norm1(&poll.question);
        b.push(
            AnchorPart::Footer,
            vec![Seg::new(
                text::truncate(&q, usize::from(t), g.ellipsis),
                Sem::EmbedTitle,
            )],
        );
        let bar_w = (usize::from(t) / 3).clamp(4, 20);
        for a in &poll.answers {
            let pct = if poll.total_votes == 0 {
                0.0
            } else {
                a.votes as f32 / poll.total_votes as f32
            };
            let label_w = usize::from(t).saturating_sub(bar_w + 7).max(1);
            let label = text::fit(&b.norm1(&a.text), label_w);
            let bar = crate::meters::bar(pct, bar_w, p.ctx.charset);
            b.push(
                AnchorPart::Footer,
                vec![
                    Seg::new(
                        label,
                        if a.me_voted {
                            Sem::Accent
                        } else {
                            Sem::Text(MdStyle::default())
                        },
                    ),
                    Seg::new(" ", Sem::Dim),
                    Seg::new(bar, Sem::Bar),
                    Seg::new(format!(" {:>3}%", (pct * 100.0).round() as u32), Sem::Dim),
                ],
            );
        }
    }

    // Reactions.
    if !m.reactions.is_empty() {
        let mut cur: Vec<Seg> = Vec::new();
        let mut w = 0usize;
        for r in &m.reactions {
            let emoji = if r.emoji_id.is_some() {
                format!(":{}:", r.emoji_name)
            } else {
                r.emoji_name.clone()
            };
            let chip = format!(
                "{} {}",
                text::normalize(&sanitize_single_line(&emoji), &p.ctx),
                r.count
            );
            let cw = str_width(&chip);
            if w > 0 && w + 2 + cw > usize::from(t) {
                b.push(AnchorPart::Reactions, std::mem::take(&mut cur));
                w = 0;
            }
            if w > 0 {
                cur.push(Seg::new("  ", Sem::Dim));
                w += 2;
            }
            cur.push(Seg::new(
                text::truncate(&chip, usize::from(t), g.ellipsis),
                Sem::Reaction(r.me),
            ));
            w += cw;
        }
        if !cur.is_empty() {
            b.push(AnchorPart::Reactions, cur);
        }
    }

    if b.lines.len() == body_start {
        b.push(
            AnchorPart::Content { grapheme: 0 },
            vec![Seg::new("", Sem::Dim)],
        );
    }

    // Gutter prefix on the first body line (gutter mode).
    if gutter_mode {
        let nick_w = 12usize;
        for (i, line) in b.lines.iter_mut().enumerate() {
            if i < body_start {
                continue;
            }
            if let Some(first) = line.segs.first_mut()
                && first.sem == Sem::Gutter
            {
                *first = Seg::new(String::new(), Sem::Gutter);
            }
            let mut pre = Vec::new();
            if i == body_start {
                pre.push(Seg::new(format!("{time} "), Sem::Time));
                let nick = if grouped {
                    " ".repeat(nick_w)
                } else {
                    let n = text::truncate(&nick_raw, nick_w, g.ellipsis);
                    format!("{}{n}", " ".repeat(nick_w.saturating_sub(str_width(&n))))
                };
                pre.push(Seg::new(nick, Sem::Nick(m.author.id.as_u64())));
            } else {
                pre.push(Seg::new(" ".repeat(6 + nick_w), Sem::Gutter));
            }
            pre.push(Seg::new(format!(" {} ", g.thin_v), Sem::Gutter));
            line.segs.splice(0..1.min(line.segs.len()), pre);
        }
        // Reply line sits above, indented to the text column.
    }

    for l in &mut b.lines {
        l.mention = mentions_me;
    }
    let _ = b.t;
    b.lines
}

#[allow(clippy::too_many_arguments)]
fn push_image(
    b: &mut Builder<'_>,
    idx: u16,
    url: String,
    bw: u16,
    bh: u16,
    label: String,
    animated: bool,
    sidecar: bool,
    body_start: usize,
) {
    if sidecar {
        // Attach to existing body lines from the top, padding with blank lines.
        let mut row = 0u16;
        let mut li = body_start;
        // Skip lines already carrying a side-car image.
        while li < b.lines.len() && b.lines[li].image.is_some() {
            li += 1;
        }
        while row < bh {
            if li >= b.lines.len() {
                b.push(
                    AnchorPart::Attachment {
                        index: idx,
                        permille: row * 1000 / bh.max(1),
                    },
                    Vec::new(),
                );
            }
            b.lines[li].image = Some(ImageRow {
                url: url.clone(),
                row,
                rows: bh,
                cols: bw,
                x_off: 0,
                label: label.clone(),
                animated,
                sidecar: true,
            });
            row += 1;
            li += 1;
        }
        return;
    }
    for row in 0..bh {
        let mut line = VLine::new(
            AnchorPart::Attachment {
                index: idx,
                permille: row * 1000 / bh.max(1),
            },
            vec![b.pad()],
        );
        line.image = Some(ImageRow {
            url: url.clone(),
            row,
            rows: bh,
            cols: bw,
            x_off: 0,
            label: label.clone(),
            animated,
            sidecar: false,
        });
        b.lines.push(line);
    }
}

fn layout_embed(b: &mut Builder<'_>, e: &Embed, idx: u16, body_start: usize) {
    let p = b.p;
    let g = p.glyphs;
    let t = b.t;
    let w = t.saturating_sub(2).clamp(1, 100);
    let bar = || Seg::new(format!("{} ", g.embed_bar), Sem::EmbedBar(e.color));
    let mut gidx: u32 = 0;
    let push = |b: &mut Builder<'_>, gi: u32, mut segs: Vec<Seg>| {
        segs.insert(0, bar());
        b.push(
            AnchorPart::Embed {
                index: idx,
                grapheme: gi,
            },
            segs,
        );
    };
    let count = |s: &str| {
        u32::try_from(unicode_segmentation::UnicodeSegmentation::graphemes(s, true).count())
            .unwrap_or(0)
            + 1
    };

    let media_only = matches!(
        e.kind,
        EmbedKind::Image | EmbedKind::Gifv | EmbedKind::Video
    ) && e.title.is_none()
        && e.description.is_none();
    let thumb = if !media_only && w >= 48 && !p.text_tags {
        e.thumbnail.as_ref()
    } else {
        None
    };
    let (tw, th) = if thumb.is_some() {
        let tw = (w / 3).clamp(1, 12);
        let th = ((f32::from(tw) * f32::from(p.cell_px.0.max(1)) / f32::from(p.cell_px.1.max(1)))
            .round() as u16)
            .max(1);
        (tw, th)
    } else {
        (0, 0)
    };
    let first_line = b.lines.len();
    let narrow = w.saturating_sub(tw + 1).max(1);
    let width_at = |b: &Builder<'_>| -> u16 {
        if thumb.is_some() && b.lines.len() < first_line + usize::from(th) {
            narrow
        } else {
            w
        }
    };

    if !media_only {
        if let Some(pv) = e.provider.as_ref().and_then(|p| p.name.as_deref()) {
            let s = b.norm1(pv);
            let wd = width_at(b);
            push(
                b,
                gidx,
                vec![Seg::new(
                    text::truncate(&s, usize::from(wd), g.ellipsis),
                    Sem::Dim,
                )],
            );
            gidx += count(&s);
        }
        if let Some(a) = &e.author {
            let s = b.norm1(&a.name);
            let wd = width_at(b);
            push(
                b,
                gidx,
                vec![Seg::new(
                    text::truncate(&s, usize::from(wd), g.ellipsis),
                    Sem::FieldName,
                )],
            );
            gidx += count(&s);
        }
        if let Some(title) = &e.title {
            let s = b.norm1(title);
            for l in text::wrap(&s, usize::from(width_at(b))) {
                push(
                    b,
                    gidx + l.start_grapheme,
                    vec![Seg::new(l.text, Sem::EmbedTitle)],
                );
            }
            gidx += count(&s);
        }
        if let Some(desc) = &e.description {
            let md = b.md(desc);
            let mut base = gidx;
            for ml in &md {
                let wd = width_at(b);
                let one = std::slice::from_ref(ml);
                for l in markdown::wrap_lines(one, usize::from(wd)) {
                    push(b, base + l.start_grapheme, Builder::spans_to_segs(&l.spans));
                }
                let n: u32 = ml
                    .spans
                    .iter()
                    .map(|s| {
                        u32::try_from(
                            unicode_segmentation::UnicodeSegmentation::graphemes(
                                s.text.as_str(),
                                true,
                            )
                            .count(),
                        )
                        .unwrap_or(0)
                    })
                    .sum();
                base += n + 1;
            }
            gidx = base;
        }
        // Pad to thumbnail height.
        if thumb.is_some() {
            while b.lines.len() < first_line + usize::from(th) {
                push(b, gidx, Vec::new());
            }
        }
        // Fields grid.
        let n_max = ((usize::from(w) + 2) / 18)
            .clamp(1, 3)
            .min(usize::from(p.max_inline_cols.max(1)));
        for row in e.inline_field_rows(n_max) {
            let k = row.len().max(1);
            let k16 = u16::try_from(k).unwrap_or(1);
            let gap_total = 2 * (k16 - 1);
            let fw_base = w.saturating_sub(gap_total) / k16;
            let rem = w.saturating_sub(gap_total) % k16;
            let mut cols: Vec<Vec<Seg>> = Vec::new();
            let mut col_w: Vec<u16> = Vec::new();
            let mut height = 0usize;
            let mut cells: Vec<Vec<Vec<Seg>>> = Vec::new();
            for (ci, f) in row.iter().enumerate() {
                let fw = (fw_base + u16::from(u16::try_from(ci).unwrap_or(0) < rem)).max(1);
                col_w.push(fw);
                let mut lines: Vec<Vec<Seg>> = Vec::new();
                let name = b.norm1(&f.name);
                for l in text::wrap(&name, usize::from(fw)) {
                    lines.push(vec![Seg::new(l.text, Sem::FieldName)]);
                }
                let md = b.md(&f.value);
                for l in markdown::wrap_lines(&md, usize::from(fw)) {
                    lines.push(Builder::spans_to_segs(&l.spans));
                }
                height = height.max(lines.len());
                cells.push(lines);
                gidx += count(&name);
            }
            let _ = &mut cols;
            for r in 0..height {
                let mut segs = Vec::new();
                for (ci, col) in cells.iter().enumerate() {
                    let fw = usize::from(col_w[ci]);
                    let line = col.get(r).cloned().unwrap_or_default();
                    let used: usize = line.iter().map(|s| str_width(&s.text)).sum();
                    segs.extend(line);
                    if ci + 1 < cells.len() {
                        segs.push(Seg::new(" ".repeat(fw.saturating_sub(used) + 2), Sem::Dim));
                    }
                }
                push(b, gidx, segs);
            }
        }
    }

    // Thumbnail image rows (top-right of the embed).
    if let Some(th_media) = thumb {
        let url = th_media
            .proxy_url
            .clone()
            .unwrap_or_else(|| th_media.url.clone());
        for r in 0..th {
            let li = first_line + usize::from(r);
            if let Some(line) = b.lines.get_mut(li) {
                line.image = Some(ImageRow {
                    url: url.clone(),
                    row: r,
                    rows: th,
                    cols: tw,
                    x_off: 2 + w - tw,
                    label: "[thumb]".to_string(),
                    animated: false,
                    sidecar: false,
                });
            }
        }
    }

    // Hero image / video.
    let hero = e.image.as_ref().or(if media_only {
        e.thumbnail.as_ref()
    } else {
        None
    });
    let animated = e.kind == EmbedKind::Gifv;
    if e.kind == EmbedKind::Video && e.video.is_some() && hero.is_none() {
        let s = format!(
            "[{} video {}]",
            g.play,
            e.title.as_deref().map(|t| b.norm1(t)).unwrap_or_default()
        );
        push(
            b,
            gidx,
            vec![Seg::new(
                text::truncate(&text::normalize(&s, &p.ctx), usize::from(w), g.ellipsis),
                Sem::Tag,
            )],
        );
    } else if let Some(img) = hero {
        let label = if animated {
            format!("[{}GIF]", g.play)
        } else {
            "[IMG]".to_string()
        };
        if p.text_tags {
            let dims = match (img.width, img.height) {
                (Some(a), Some(c)) => format!(" {a}x{c}"),
                _ => String::new(),
            };
            let s = format!("[{}{dims}]", if animated { "GIF" } else { "IMG" });
            push(b, gidx, vec![Seg::new(s, Sem::Tag)]);
        } else {
            let sidecar = p.measure.sidecar;
            let (max_w, on_side) = if sidecar >= 12 {
                (sidecar, true)
            } else {
                (w.min(48), false)
            };
            let (bw, bh) = image_box(
                img.width,
                img.height,
                max_w,
                p.max_img_rows.max(1),
                p.cell_px,
            );
            let url = img.proxy_url.clone().unwrap_or_else(|| img.url.clone());
            if on_side {
                push_image(b, 0, url, bw, bh, label, animated, true, body_start);
            } else {
                for r in 0..bh {
                    let mut line = VLine::new(
                        AnchorPart::Embed {
                            index: idx,
                            grapheme: gidx,
                        },
                        vec![b.pad(), bar()],
                    );
                    line.image = Some(ImageRow {
                        url: url.clone(),
                        row: r,
                        rows: bh,
                        cols: bw,
                        x_off: 2,
                        label: label.clone(),
                        animated,
                        sidecar: false,
                    });
                    b.lines.push(line);
                }
            }
        }
    }

    if !media_only {
        let mut foot = String::new();
        if let Some(f) = &e.footer {
            foot.push_str(&b.norm1(&f.text));
        }
        if let Some(ts) = &e.timestamp {
            if !foot.is_empty() {
                foot.push_str(&format!(" {} ", g.dot));
            }
            foot.push_str(&b.norm1(ts.get(..16).unwrap_or(ts)).replace('T', " "));
        }
        if !foot.is_empty() {
            push(
                b,
                gidx,
                vec![Seg::new(
                    text::truncate(&foot, usize::from(w), g.ellipsis),
                    Sem::Dim,
                )],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_box_never_zero() {
        assert_eq!(image_box(Some(10_000), Some(1), 48, 16, (8, 16)).1, 1);
        assert_eq!(image_box(Some(1), Some(10_000), 48, 16, (8, 16)).0, 1);
        let (w, h) = image_box(Some(1280), Some(720), 48, 16, (8, 16));
        assert!(w <= 48 && h <= 16 && w >= 1 && h >= 1);
        assert_eq!(image_box(None, None, 0, 0, (0, 0)), (1, 1));
    }

    #[test]
    fn time_format() {
        let id = Snowflake::new(175_928_847_299_117_063);
        assert_eq!(fmt_time(id, 0).len(), 5);
    }
}
