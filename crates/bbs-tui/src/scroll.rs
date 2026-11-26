//! Anchor-based message viewport (§5): bottom-visible-line anchoring, O(visible) reflow,
//! and a per-message layout cache keyed by everything that affects wrapping.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::Arc;

use bbs_core::{AnchorPart, Message, ScrollAnchor, ScrollState, Snowflake};
use lru::LruCache;

use crate::messages::{LayoutParams, Resolve, VLine, is_grouped, layout_message};

/// Cache capacity (§5).
pub const CACHE_ENTRIES: usize = 4096;

/// Per-message layout cache.
pub struct LayoutCache {
    lru: LruCache<u64, Arc<Vec<VLine>>>,
    /// Hits (diagnostics).
    pub hits: u64,
    /// Misses (diagnostics).
    pub misses: u64,
}

impl std::fmt::Debug for LayoutCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutCache")
            .field("len", &self.lru.len())
            .finish()
    }
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            lru: LruCache::new(NonZeroUsize::new(CACHE_ENTRIES).unwrap_or(NonZeroUsize::MIN)),
            hits: 0,
            misses: 0,
        }
    }
}

/// Hash of layout parameters (width class key).
#[must_use]
pub fn params_key(p: &LayoutParams) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.width.hash(&mut h);
    p.measure.gutter.hash(&mut h);
    p.measure.indent.hash(&mut h);
    p.measure.text.hash(&mut h);
    p.measure.sidecar.hash(&mut h);
    format!("{:?}{:?}{}", p.ctx.charset, p.ctx.emoji, p.ctx.lcd_a02).hash(&mut h);
    p.glyphs.tl.hash(&mut h);
    p.max_inline_cols.hash(&mut h);
    p.cell_px.hash(&mut h);
    p.max_img_rows.hash(&mut h);
    p.utc_offset_min.hash(&mut h);
    p.text_tags.hash(&mut h);
    h.finish()
}

impl LayoutCache {
    /// Clear everything (e.g. after a mention-resolution change).
    pub fn clear(&mut self) {
        self.lru.clear();
    }

    /// Lines for a message.
    #[allow(clippy::too_many_arguments)]
    pub fn lines(
        &mut self,
        m: &Message,
        grouped: bool,
        mentions_me: bool,
        p: &LayoutParams,
        pkey: u64,
        res: &dyn Resolve,
        playback: Option<&bbs_core::VoicePlaybackState>,
    ) -> Arc<Vec<VLine>> {
        let playback = playback.filter(|pb| pb.message_id == m.id);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        m.id.hash(&mut h);
        m.edited_timestamp.hash(&mut h);
        m.content.hash(&mut h);
        m.pinned.hash(&mut h);
        if let Some(poll) = &m.poll {
            poll.total_votes.hash(&mut h);
            for a in &poll.answers {
                a.votes.hash(&mut h);
                a.me_voted.hash(&mut h);
            }
        }
        if let Some(pb) = playback {
            pb.attachment_id.hash(&mut h);
            pb.playing.hash(&mut h);
            ((pb.position_secs.max(0.0) * 4.0) as u64).hash(&mut h);
            (pb.duration_secs.max(0.0) as u64).hash(&mut h);
        }
        m.reactions.len().hash(&mut h);
        for r in &m.reactions {
            r.count.hash(&mut h);
            r.me.hash(&mut h);
        }
        for a in &m.attachments {
            a.url.hash(&mut h);
        }
        m.embeds.len().hash(&mut h);
        grouped.hash(&mut h);
        mentions_me.hash(&mut h);
        pkey.hash(&mut h);
        let key = h.finish();
        if let Some(v) = self.lru.get(&key) {
            self.hits += 1;
            return Arc::clone(v);
        }
        self.misses += 1;
        let v = Arc::new(layout_message(m, grouped, mentions_me, p, res, playback));
        self.lru.put(key, Arc::clone(&v));
        v
    }
}

/// Scroll state plus a width-scoped line hint (derived; never part of the anchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scroll {
    /// Persistent state (the contract value).
    pub state: ScrollState,
    /// `(line index within the anchored message, params key)`.
    pub hint: Option<(usize, u64)>,
}

/// One visible line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewLine {
    /// Owning message.
    pub message_id: Snowflake,
    /// Line index inside the message.
    pub line_idx: usize,
    /// The line.
    pub line: VLine,
}

/// Rendered viewport (top → bottom).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// Visible lines (≤ height).
    pub lines: Vec<ViewLine>,
    /// The first loaded line is visible (request older history).
    pub at_top: bool,
    /// Following the tail.
    pub at_bottom: bool,
    /// Estimated lines above the viewport (for the scrollbar), capped.
    pub above: usize,
    /// Estimated lines below.
    pub below: usize,
}

/// Inputs for timeline operations.
pub struct Timeline<'a> {
    /// Messages, chronological.
    pub msgs: &'a VecDeque<Message>,
    /// Layout params.
    pub params: LayoutParams,
    /// Params key.
    pub pkey: u64,
    /// Mention resolver.
    pub res: &'a dyn Resolve,
    /// Mentions-me predicate.
    pub mentions: &'a dyn Fn(&Message) -> bool,
    /// Active voice-message playback (progress rendering).
    pub playback: Option<&'a bbs_core::VoicePlaybackState>,
}

/// Position of a line: (message index, line index).
type Pos = (usize, usize);

fn part_matches(a: &AnchorPart, b: &AnchorPart) -> Option<u32> {
    // Returns the comparable offset when both are the same variant/index.
    match (a, b) {
        (AnchorPart::Header, AnchorPart::Header)
        | (AnchorPart::Reactions, AnchorPart::Reactions)
        | (AnchorPart::Footer, AnchorPart::Footer) => Some(0),
        (AnchorPart::Content { grapheme: x }, AnchorPart::Content { .. }) => Some(*x),
        (
            AnchorPart::Embed {
                index: i,
                grapheme: x,
            },
            AnchorPart::Embed { index: j, .. },
        ) if i == j => Some(*x),
        (
            AnchorPart::Attachment {
                index: i,
                permille: x,
            },
            AnchorPart::Attachment { index: j, .. },
        ) if i == j => Some(u32::from(*x)),
        _ => None,
    }
}

impl Timeline<'_> {
    fn lines(&self, cache: &mut LayoutCache, i: usize) -> Arc<Vec<VLine>> {
        let m = &self.msgs[i];
        let prev = if i > 0 { self.msgs.get(i - 1) } else { None };
        let grouped = is_grouped(prev, m);
        cache.lines(
            m,
            grouped,
            (self.mentions)(m),
            &self.params,
            self.pkey,
            self.res,
            self.playback,
        )
    }

    /// Message index for an id, falling back to the nearest older surviving message.
    fn msg_index(&self, id: Snowflake) -> Option<usize> {
        if self.msgs.is_empty() {
            return None;
        }
        if let Some(i) = self.msgs.iter().position(|m| m.id == id) {
            return Some(i);
        }
        Some(
            self.msgs
                .iter()
                .rposition(|m| m.id.as_u64() < id.as_u64())
                .unwrap_or(0),
        )
    }

    /// Resolve the bottom anchor position.
    fn resolve(&self, cache: &mut LayoutCache, scroll: &Scroll) -> Option<Pos> {
        let last = self.msgs.len().checked_sub(1)?;
        match scroll.state {
            ScrollState::FollowTail => {
                let n = self.lines(cache, last).len();
                Some((last, n.saturating_sub(1)))
            }
            ScrollState::Anchored(a) => {
                let mi = self.msg_index(a.message_id)?;
                let lines = self.lines(cache, mi);
                if self.msgs[mi].id != a.message_id {
                    return Some((mi, lines.len().saturating_sub(1)));
                }
                if let Some((h, k)) = scroll.hint
                    && k == self.pkey
                    && lines.get(h).is_some_and(|l| l.part == a.part)
                {
                    return Some((mi, h));
                }
                let target = part_offset(&a.part);
                let mut best: Option<usize> = None;
                for (li, l) in lines.iter().enumerate() {
                    if part_matches(&l.part, &a.part).is_some()
                        && (part_offset(&l.part) <= target || best.is_none())
                    {
                        best = Some(li);
                    }
                }
                Some((mi, best.unwrap_or(lines.len().saturating_sub(1))))
            }
        }
    }

    /// Build the view for `height` rows.
    pub fn view(&self, cache: &mut LayoutCache, scroll: &Scroll, height: u16) -> View {
        let height = usize::from(height);
        let Some((mut mi, li)) = self.resolve(cache, scroll) else {
            return View {
                at_top: true,
                at_bottom: true,
                ..View::default()
            };
        };
        let mut out: Vec<ViewLine> = Vec::with_capacity(height);
        let mut lines = self.lines(cache, mi);
        let mut li = li as isize;
        let mut at_top = false;
        while out.len() < height {
            if li < 0 {
                if mi == 0 {
                    at_top = true;
                    break;
                }
                mi -= 1;
                lines = self.lines(cache, mi);
                li = lines.len() as isize - 1;
                continue;
            }
            let idx = li as usize;
            if let Some(l) = lines.get(idx) {
                out.push(ViewLine {
                    message_id: self.msgs[mi].id,
                    line_idx: idx,
                    line: l.clone(),
                });
            }
            li -= 1;
        }
        if !at_top && li < 0 && mi == 0 {
            at_top = true;
        }
        out.reverse();
        // Rough estimate of lines above/below for the scrollbar.
        let top_mi = out.first().map_or(0, |v| {
            self.msgs
                .iter()
                .position(|m| m.id == v.message_id)
                .unwrap_or(0)
        });
        let bot_mi = out.last().map_or(0, |v| {
            self.msgs
                .iter()
                .position(|m| m.id == v.message_id)
                .unwrap_or(0)
        });
        let above = top_mi * 2;
        let below = self.msgs.len().saturating_sub(bot_mi + 1) * 2;
        View {
            lines: out,
            at_top,
            at_bottom: matches!(scroll.state, ScrollState::FollowTail),
            above,
            below,
        }
    }

    /// Scroll by `delta` lines (negative = up/older). Returns the new scroll.
    pub fn scroll_by(
        &self,
        cache: &mut LayoutCache,
        scroll: &Scroll,
        delta: isize,
        height: u16,
    ) -> Scroll {
        let Some((mut mi, li)) = self.resolve(cache, scroll) else {
            return *scroll;
        };
        let mut li = li as isize;
        let mut lines = self.lines(cache, mi);
        let last = self.msgs.len() - 1;
        if delta > 0 {
            let mut d = delta;
            while d > 0 {
                if (li as usize) + 1 < lines.len() {
                    li += 1;
                } else if mi < last {
                    mi += 1;
                    lines = self.lines(cache, mi);
                    li = 0;
                } else {
                    return Scroll {
                        state: ScrollState::FollowTail,
                        hint: None,
                    };
                }
                d -= 1;
            }
            if mi == last && (li as usize) + 1 >= lines.len() {
                return Scroll {
                    state: ScrollState::FollowTail,
                    hint: None,
                };
            }
        } else {
            let mut d = -delta;
            while d > 0 {
                if li > 0 {
                    li -= 1;
                } else if mi > 0 {
                    mi -= 1;
                    lines = self.lines(cache, mi);
                    li = lines.len() as isize - 1;
                } else {
                    break;
                }
                d -= 1;
            }
            // Clamp: keep at least `height` lines from the top (bottom anchor ≥ height-1).
            let mut count = 0usize;
            let (mut cm, mut cl) = (mi, li);
            loop {
                count += 1;
                if count >= usize::from(height.max(1)) {
                    break;
                }
                if cl > 0 {
                    cl -= 1;
                } else if cm > 0 {
                    cm -= 1;
                    cl = self.lines(cache, cm).len() as isize - 1;
                } else {
                    break;
                }
            }
            if count < usize::from(height.max(1)) {
                return self.top_scroll(cache, height);
            }
        }
        self.anchor_at(cache, mi, li.max(0) as usize)
    }

    /// Scroll showing the very first loaded line at the top of a `height`-row viewport.
    pub fn top_scroll(&self, cache: &mut LayoutCache, height: u16) -> Scroll {
        if self.msgs.is_empty() {
            return Scroll::default();
        }
        let mut remaining = usize::from(height.max(1)) - 1;
        let (mut mi, mut li) = (0usize, 0usize);
        loop {
            let n = self.lines(cache, mi).len();
            if li + remaining < n {
                li += remaining;
                break;
            }
            remaining -= n - li;
            if mi + 1 >= self.msgs.len() {
                return Scroll::default();
            }
            mi += 1;
            li = 0;
        }
        self.anchor_at(cache, mi, li)
    }

    /// Scroll that anchors the bottom at `(mi, li)`.
    pub fn anchor_at(&self, cache: &mut LayoutCache, mi: usize, li: usize) -> Scroll {
        if mi >= self.msgs.len() {
            return Scroll::default();
        }
        let lines = self.lines(cache, mi);
        let part = lines.get(li).map_or(AnchorPart::Header, |l| l.part);
        Scroll {
            state: ScrollState::Anchored(ScrollAnchor {
                message_id: self.msgs[mi].id,
                part,
            }),
            hint: Some((li, self.pkey)),
        }
    }

    /// Ensure message `id` is visible (anchor to its last line, or to `height−1` lines below
    /// its first line when taller than the viewport).
    pub fn reveal(&self, cache: &mut LayoutCache, id: Snowflake, height: u16) -> Scroll {
        let Some(mi) = self.msgs.iter().position(|m| m.id == id) else {
            return Scroll::default();
        };
        let n = self.lines(cache, mi).len();
        let li = n.saturating_sub(1).min(usize::from(height.max(1)) - 1);
        if mi + 1 == self.msgs.len() && li + 1 == n {
            return Scroll::default();
        }
        self.anchor_at(cache, mi, li)
    }
}

fn part_offset(p: &AnchorPart) -> u32 {
    match p {
        AnchorPart::Content { grapheme } | AnchorPart::Embed { grapheme, .. } => *grapheme,
        AnchorPart::Attachment { permille, .. } => u32::from(*permille),
        _ => 0,
    }
}
