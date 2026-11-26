//! PTY byte-rate governor (§6.6, I9): accounts every flush and tells producers what to drop.

use std::collections::VecDeque;

/// Output classes in drop order (first dropped first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputClass {
    /// Inline video frames.
    Video,
    /// GIF animation frames.
    Animation,
    /// VU meter updates.
    Meters,
    /// Sparklines.
    Sparklines,
    /// Image (re)sends.
    Images,
    /// Text/UI — never dropped.
    Text,
}

/// Governor decision for a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Allow video frames.
    pub video: bool,
    /// Video fps step (10 → 5 → 2 → 0 keyframe-only).
    pub video_fps: u8,
    /// Allow animation frames.
    pub animation: bool,
    /// VU meter Hz (10 → 4 → 1).
    pub meter_hz: u8,
    /// Allow sparkline updates.
    pub sparklines: bool,
    /// Allow image re-sends (else defer).
    pub images: bool,
}

/// Sliding-window (1 s) byte-rate governor.
#[derive(Debug, Clone)]
pub struct Governor {
    limit: u64,
    window: VecDeque<(u64, u64)>,
    total: u64,
}

impl Governor {
    /// `256 KiB/s` local, `48 KiB/s` over SSH.
    #[must_use]
    pub fn new(over_ssh: bool) -> Self {
        Self::with_limit(if over_ssh { 48 * 1024 } else { 256 * 1024 })
    }

    /// Explicit bytes/second limit.
    #[must_use]
    pub fn with_limit(limit: u64) -> Self {
        Self {
            limit: limit.max(1),
            window: VecDeque::new(),
            total: 0,
        }
    }

    /// Record `bytes` flushed at `now_ms`.
    pub fn record(&mut self, now_ms: u64, bytes: u64) {
        self.window.push_back((now_ms, bytes));
        self.total += bytes;
        self.expire(now_ms);
    }

    fn expire(&mut self, now_ms: u64) {
        while let Some((t, b)) = self.window.front().copied() {
            if now_ms.saturating_sub(t) < 1000 {
                break;
            }
            self.window.pop_front();
            self.total = self.total.saturating_sub(b);
        }
    }

    /// Bytes in the last second.
    #[must_use]
    pub fn rate(&self) -> u64 {
        self.total
    }

    /// Budget for the next frame at `now_ms`.
    pub fn budget(&mut self, now_ms: u64) -> Budget {
        self.expire(now_ms);
        let load = self.total as f64 / self.limit as f64;
        Budget {
            video: load < 0.9,
            video_fps: if load < 0.5 {
                10
            } else if load < 0.7 {
                5
            } else if load < 0.9 {
                2
            } else {
                0
            },
            animation: load < 0.75,
            meter_hz: if load < 0.6 {
                10
            } else if load < 0.9 {
                4
            } else {
                1
            },
            sparklines: load < 0.95,
            images: load < 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degrades_in_order() {
        let mut g = Governor::with_limit(1000);
        assert!(g.budget(0).video);
        g.record(0, 950);
        let b = g.budget(10);
        assert!(!b.video && !b.animation && b.images);
        assert_eq!(b.meter_hz, 1);
        let b = g.budget(1500);
        assert!(b.video);
    }
}
