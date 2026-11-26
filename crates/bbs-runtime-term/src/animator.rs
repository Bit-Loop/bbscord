//! Adaptive animated GIF / inline video frame pacer and `100 ms` `SIGWINCH` resize debouncer.
//!
//! Decouples 60+ fps immediate text layout reflow from debounced bitmap re-scaling so window drags
//! never trigger redundant image re-encodes or network downloads, while capping animation FPS by
//! graphics tier and SSH transport latency so slow PTYs never backlog.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bbs_core::model::RgbaFrame;
use bbs_core::viewport::{GraphicsTier, TerminalCapabilities, ViewportGeometry};

use crate::media::{DEFAULT_GIF_DELAY_MS, DecodedMedia};

/// Default quiet period before re-scaling bitmaps after `SIGWINCH` settles (`100 ms`).
pub const DEFAULT_RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

/// Default quiet period before upgrading scroll-proxy HalfBlocks back to Sixel/iTerm2/KittyDirect.
pub const DEFAULT_SCROLL_SETTLE: Duration = Duration::from_millis(150);

/// Local Kitty animation frame cap (`12 fps`).
pub const KITTY_LOCAL_MAX_FPS: u8 = 12;

/// Sixel or SSH (`$SSH_CONNECTION`) animation frame cap (`6 fps`).
pub const SIXEL_OR_SSH_MAX_FPS: u8 = 6;

/// Local iTerm2 / HalfBlock client-side animation frame cap (`8 fps`).
pub const FALLBACK_LOCAL_MAX_FPS: u8 = 8;

/// Immediate result of ingesting a `SIGWINCH` resize event into [`ResizeDebouncer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeUpdate {
    /// Latest viewport geometry — apply IMMEDIATELY for 60+ fps text layout reflow.
    pub text_geometry: ViewportGeometry,
    /// `true` while bitmap media re-scaling is deferred waiting for the debounce window to settle.
    pub media_rescale_pending: bool,
    /// Deadline at which bitmap re-scaling will fire if no newer resize arrives.
    pub settle_at: Option<Instant>,
}

/// Settled geometry emitted once after `SIGWINCH` remains quiet for the debounce window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledResize {
    /// Final settled viewport geometry for bitmap re-scaling from the in-RAM [`crate::media::MediaFrameCache`].
    pub geometry: ViewportGeometry,
    /// Number of raw resize events coalesced into this single bitmap re-scale.
    pub coalesced_events: u64,
}

/// `100 ms` `SIGWINCH` resize debouncer separating immediate text reflow from bitmap re-scaling.
#[derive(Debug, Clone)]
pub struct ResizeDebouncer {
    debounce_window: Duration,
    committed_media_geometry: ViewportGeometry,
    latest_geometry: ViewportGeometry,
    pending_settle_at: Option<Instant>,
    coalesced_events: u64,
}

impl ResizeDebouncer {
    /// Create a debouncer initialized at `initial_geometry` with a custom settle window.
    #[must_use]
    pub const fn new(initial_geometry: ViewportGeometry, debounce_window: Duration) -> Self {
        Self {
            debounce_window,
            committed_media_geometry: initial_geometry,
            latest_geometry: initial_geometry,
            pending_settle_at: None,
            coalesced_events: 0,
        }
    }

    /// Create a debouncer initialized at `initial_geometry` with the default `100 ms` settle window.
    #[must_use]
    pub const fn with_default_window(initial_geometry: ViewportGeometry) -> Self {
        Self::new(initial_geometry, DEFAULT_RESIZE_DEBOUNCE)
    }

    /// Configured debounce quiet window.
    #[must_use]
    pub const fn debounce_window(&self) -> Duration {
        self.debounce_window
    }

    /// Latest viewport geometry (updated immediately on every `SIGWINCH` for text reflow).
    #[must_use]
    pub const fn text_geometry(&self) -> ViewportGeometry {
        self.latest_geometry
    }

    /// Last viewport geometry committed to bitmap media re-scaling.
    #[must_use]
    pub const fn committed_media_geometry(&self) -> ViewportGeometry {
        self.committed_media_geometry
    }

    /// Returns `true` while a resize storm is active and bitmap re-scaling is deferred.
    #[must_use]
    pub const fn is_debouncing(&self) -> bool {
        self.pending_settle_at.is_some()
    }

    /// Next instant when [`Self::poll_settled`] should be called (for zero-CPU Tokio `sleep_until`).
    #[must_use]
    pub const fn next_deadline(&self) -> Option<Instant> {
        self.pending_settle_at
    }

    /// Ingest a `SIGWINCH` resize event at timestamp `now`.
    ///
    /// Text layouts should reflow immediately using [`ResizeUpdate::text_geometry`], while bitmap
    /// re-scaling waits until [`Self::poll_settled`] fires after `debounce_window` elapses.
    pub fn on_resize(&mut self, geometry: ViewportGeometry, now: Instant) -> ResizeUpdate {
        self.latest_geometry = geometry;
        self.coalesced_events = self.coalesced_events.saturating_add(1);

        if geometry == self.committed_media_geometry {
            // Returned to the already-committed bitmap geometry before settling — cancel pending rescale!
            self.pending_settle_at = None;
            self.coalesced_events = 0;
            return ResizeUpdate {
                text_geometry: geometry,
                media_rescale_pending: false,
                settle_at: None,
            };
        }

        let settle_at = now + self.debounce_window;
        self.pending_settle_at = Some(settle_at);
        ResizeUpdate {
            text_geometry: geometry,
            media_rescale_pending: true,
            settle_at: Some(settle_at),
        }
    }

    /// Convenience wrapper using `Instant::now()`.
    pub fn on_resize_now(&mut self, geometry: ViewportGeometry) -> ResizeUpdate {
        self.on_resize(geometry, Instant::now())
    }

    /// Poll whether the debounce quiet window has elapsed at timestamp `now`.
    ///
    /// Fires at most once per resize burst and returns `None` until `now >= settle_at`.
    pub fn poll_settled(&mut self, now: Instant) -> Option<SettledResize> {
        let deadline = self.pending_settle_at?;
        if now < deadline {
            return None;
        }

        self.pending_settle_at = None;
        let coalesced = self.coalesced_events;
        self.coalesced_events = 0;

        if self.latest_geometry == self.committed_media_geometry {
            return None;
        }

        self.committed_media_geometry = self.latest_geometry;
        Some(SettledResize {
            geometry: self.latest_geometry,
            coalesced_events: coalesced,
        })
    }

    /// Convenience wrapper using `Instant::now()`.
    pub fn poll_settled_now(&mut self) -> Option<SettledResize> {
        self.poll_settled(Instant::now())
    }

    /// Immediately commit `geometry` (for example after `SIGCONT` synthetic resume repaint).
    pub fn commit_immediate(&mut self, geometry: ViewportGeometry) {
        self.latest_geometry = geometry;
        self.committed_media_geometry = geometry;
        self.pending_settle_at = None;
        self.coalesced_events = 0;
    }
}

/// Adaptive animation frame pacer capping GIF / inline video update cadence by graphics tier
/// and SSH transport (`12 fps` local Kitty, `6 fps` Sixel / `$SSH_CONNECTION`, `0 fps` TextTagOnly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePacer {
    base_max_fps: u8,
    throttle_divisor: u8,
}

impl FramePacer {
    /// Construct a pacer with an explicit base FPS ceiling (`0` disables animation).
    #[must_use]
    pub const fn new(base_max_fps: u8) -> Self {
        Self {
            base_max_fps,
            throttle_divisor: 1,
        }
    }

    /// Derive the adaptive FPS cap from negotiated [`TerminalCapabilities`].
    #[must_use]
    pub const fn from_capabilities(caps: &TerminalCapabilities) -> Self {
        Self::for_tier(caps.graphics_tier, caps.over_ssh)
    }

    /// Derive the adaptive FPS cap for a specific [`GraphicsTier`] and SSH flag.
    #[must_use]
    pub const fn for_tier(tier: GraphicsTier, over_ssh: bool) -> Self {
        if matches!(tier, GraphicsTier::TextTagOnly) {
            return Self::new(0);
        }
        if over_ssh || matches!(tier, GraphicsTier::Sixel) {
            return Self::new(SIXEL_OR_SSH_MAX_FPS);
        }
        match tier {
            GraphicsTier::KittyPlaceholder | GraphicsTier::KittyDirect => {
                Self::new(KITTY_LOCAL_MAX_FPS)
            }
            GraphicsTier::ITerm2 | GraphicsTier::HalfBlock => Self::new(FALLBACK_LOCAL_MAX_FPS),
            GraphicsTier::Sixel => Self::new(SIXEL_OR_SSH_MAX_FPS),
            GraphicsTier::TextTagOnly => Self::new(0),
        }
    }

    /// Apply a dynamic governor backoff divisor (`1` = full rate, `2` = half rate, clamped `1..=6`).
    pub fn set_throttle_divisor(&mut self, divisor: u8) {
        self.throttle_divisor = divisor.clamp(1, 6);
    }

    /// Base FPS ceiling before dynamic throttling.
    #[must_use]
    pub const fn base_max_fps(&self) -> u8 {
        self.base_max_fps
    }

    /// Active effective FPS ceiling after dynamic throttling (`0` when animation disabled).
    #[must_use]
    pub const fn max_fps(&self) -> u8 {
        if self.base_max_fps == 0 {
            return 0;
        }
        let div = if self.throttle_divisor == 0 {
            1
        } else {
            self.throttle_divisor
        };
        let divided = self.base_max_fps / div;
        if divided == 0 { 1 } else { divided }
    }

    /// Returns `true` when animation updates are permitted.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.max_fps() > 0
    }

    /// Minimum inter-frame duration enforced by this pacer (`None` when disabled).
    #[must_use]
    pub const fn min_frame_interval(&self) -> Option<Duration> {
        let fps = self.max_fps();
        if fps == 0 {
            None
        } else {
            Some(Duration::from_nanos(1_000_000_000 / (fps as u64)))
        }
    }

    /// Compute the effective display duration for a frame whose intrinsic GIF delay is
    /// `intrinsic_delay` (never faster than [`Self::min_frame_interval`]).
    #[must_use]
    pub fn effective_frame_delay(&self, intrinsic_delay: Duration) -> Option<Duration> {
        let min_interval = self.min_frame_interval()?;
        Some(if intrinsic_delay > min_interval {
            intrinsic_delay
        } else {
            min_interval
        })
    }
}

/// Stateful per-GIF playback controller combining focus gating, frame delays, and [`FramePacer`].
#[derive(Debug, Clone)]
pub struct GifAnimationPlayer {
    media: Arc<DecodedMedia>,
    frame_index: usize,
    last_advance: Option<Instant>,
    focused: bool,
    playing: bool,
}

impl GifAnimationPlayer {
    /// Create a new animation player starting cleanly at frame `0`.
    #[must_use]
    pub fn new(media: Arc<DecodedMedia>, focused: bool) -> Self {
        Self {
            media,
            frame_index: 0,
            last_advance: None,
            focused,
            playing: true,
        }
    }

    /// Borrow the underlying [`DecodedMedia`].
    #[must_use]
    pub fn media(&self) -> &Arc<DecodedMedia> {
        &self.media
    }

    /// Active frame index (`0` when unfocused or static).
    #[must_use]
    pub const fn frame_index(&self) -> usize {
        self.frame_index
    }

    /// Borrow the currently selected [`RgbaFrame`].
    #[must_use]
    pub fn current_frame(&self) -> Option<&RgbaFrame> {
        self.media.frame(self.frame_index)
    }

    /// Update focus state; losing focus resets display to static frame `0` (zero background ticks).
    pub fn set_focused(&mut self, focused: bool) {
        if self.focused != focused {
            self.focused = focused;
            if !focused {
                self.frame_index = 0;
                self.last_advance = None;
            }
        }
    }

    /// Pause or resume playback.
    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
        if !playing {
            self.last_advance = None;
        }
    }

    /// Returns `true` when this player should actively animate under `pacer`.
    #[must_use]
    pub fn is_animating(&self, pacer: &FramePacer) -> bool {
        self.focused && self.playing && self.media.is_animated && self.media.frame_count() > 1 && pacer.is_enabled()
    }

    fn current_frame_intrinsic_delay(&self) -> Duration {
        let ms = self
            .current_frame()
            .map(|f| if f.delay_ms == 0 { DEFAULT_GIF_DELAY_MS } else { f.delay_ms })
            .unwrap_or(DEFAULT_GIF_DELAY_MS);
        Duration::from_millis(u64::from(ms))
    }

    /// Next instant at which this player should advance to the next frame (for Tokio `sleep_until`).
    #[must_use]
    pub fn next_tick_at(&self, pacer: &FramePacer) -> Option<Instant> {
        if !self.is_animating(pacer) {
            return None;
        }
        let delay = pacer.effective_frame_delay(self.current_frame_intrinsic_delay())?;
        self.last_advance.map(|prev| prev + delay)
    }

    /// Advance animation state at timestamp `now`, returning `Some(new_frame_index)` only when a
    /// new frame should be rendered.
    pub fn tick(&mut self, now: Instant, pacer: &FramePacer) -> Option<usize> {
        if !self.is_animating(pacer) {
            return None;
        }

        let delay = pacer.effective_frame_delay(self.current_frame_intrinsic_delay())?;
        match self.last_advance {
            None => {
                // Anchor initial frame timestamp without skipping frame 0 immediately.
                self.last_advance = Some(now);
                None
            }
            Some(prev) => {
                if now.saturating_duration_since(prev) >= delay {
                    self.frame_index = (self.frame_index + 1) % self.media.frame_count();
                    self.last_advance = Some(now);
                    Some(self.frame_index)
                } else {
                    None
                }
            }
        }
    }
}

/// Scroll-proxy settle timer for non-placeholder pixel tiers (`KittyDirect`, `Sixel`, `ITerm2`).
///
/// While the user actively scrolls, boxes render instant `HalfBlock` proxies; once scrolling has
/// been still for `settle_window` (default `150 ms`), full pixel transmissions resume.
#[derive(Debug, Clone)]
pub struct ScrollProxyDebouncer {
    settle_window: Duration,
    last_scroll: Option<Instant>,
}

impl ScrollProxyDebouncer {
    /// Create a scroll-proxy debouncer with a custom settle period.
    #[must_use]
    pub const fn new(settle_window: Duration) -> Self {
        Self {
            settle_window,
            last_scroll: None,
        }
    }

    /// Create a debouncer with the default `150 ms` settle period.
    #[must_use]
    pub const fn with_default_window() -> Self {
        Self::new(DEFAULT_SCROLL_SETTLE)
    }

    /// Record a viewport scroll event at `now`.
    pub fn on_scroll(&mut self, now: Instant) {
        self.last_scroll = Some(now);
    }

    /// Returns `true` if `tier` should currently render a `HalfBlock` scroll proxy instead of
    /// transmitting a full pixel image at `now`.
    #[must_use]
    pub fn should_use_proxy(&self, tier: GraphicsTier, now: Instant) -> bool {
        if !matches!(
            tier,
            GraphicsTier::KittyDirect | GraphicsTier::Sixel | GraphicsTier::ITerm2
        ) {
            return false;
        }
        match self.last_scroll {
            Some(ts) => now.saturating_duration_since(ts) < self.settle_window,
            None => false,
        }
    }

    /// Next instant when the scroll proxy finishes settling.
    #[must_use]
    pub fn settle_deadline(&self) -> Option<Instant> {
        self.last_scroll.map(|ts| ts + self.settle_window)
    }
}

impl Default for ScrollProxyDebouncer {
    fn default() -> Self {
        Self::with_default_window()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_animated_media(frame_count: usize, delay_ms: u16) -> Arc<DecodedMedia> {
        let frames = (0..frame_count)
            .map(|i| RgbaFrame {
                width: 8,
                height: 8,
                delay_ms,
                pixels: vec![i as u8; 8 * 8 * 4],
            })
            .collect();
        Arc::new(DecodedMedia::from_rgba_frames(
            "gif://test",
            Some("test".to_owned()),
            8,
            8,
            frames,
        ))
    }

    #[test]
    fn resize_debouncer_reflows_text_immediately_and_debounces_bitmaps_100ms() {
        let t0 = Instant::now();
        let initial = ViewportGeometry::new(80, 24);
        let mut debouncer = ResizeDebouncer::with_default_window(initial);
        assert_eq!(debouncer.debounce_window(), Duration::from_millis(100));
        assert!(!debouncer.is_debouncing());

        // Rapid SIGWINCH storm (simulating 60+ fps window drag: 16 ms cadence).
        let g1 = ViewportGeometry::new(90, 26);
        let u1 = debouncer.on_resize(g1, t0);
        assert_eq!(u1.text_geometry, g1);
        assert!(u1.media_rescale_pending);
        assert_eq!(debouncer.text_geometry(), g1);
        assert_eq!(debouncer.committed_media_geometry(), initial);

        let g2 = ViewportGeometry::new(110, 32);
        let u2 = debouncer.on_resize(g2, t0 + Duration::from_millis(30));
        assert_eq!(u2.text_geometry, g2);
        assert!(debouncer.poll_settled(t0 + Duration::from_millis(80)).is_none());

        let g3 = ViewportGeometry::new(140, 40);
        let _ = debouncer.on_resize(g3, t0 + Duration::from_millis(70));
        // 99 ms after the last resize -> STILL not settled!
        assert!(debouncer.poll_settled(t0 + Duration::from_millis(169)).is_none());
        assert!(debouncer.is_debouncing());

        // Exactly 100 ms after the last resize -> settles ONCE with coalesced count = 3.
        let settled = debouncer.poll_settled(t0 + Duration::from_millis(170));
        assert_eq!(
            settled,
            Some(SettledResize {
                geometry: g3,
                coalesced_events: 3,
            })
        );
        assert!(!debouncer.is_debouncing());
        assert_eq!(debouncer.committed_media_geometry(), g3);

        // Subsequent poll does NOT re-fire.
        assert!(debouncer.poll_settled(t0 + Duration::from_millis(300)).is_none());
    }

    #[test]
    fn resize_debouncer_cancels_when_returning_to_committed_geometry_and_detects_pixel_zoom() {
        let t0 = Instant::now();
        let start = ViewportGeometry::with_pixels(100, 30, 800, 480);
        let mut debouncer = ResizeDebouncer::with_default_window(start);

        // Drag away and back before 100 ms -> zero bitmap rescale needed.
        let _ = debouncer.on_resize(ViewportGeometry::with_pixels(120, 35, 960, 560), t0);
        let back = debouncer.on_resize(start, t0 + Duration::from_millis(40));
        assert!(!back.media_rescale_pending);
        assert!(debouncer.poll_settled(t0 + Duration::from_millis(200)).is_none());

        // Font zoom (same cols/rows, different pixel dims) MUST trigger settled rescale!
        let zoomed = ViewportGeometry::with_pixels(100, 30, 1000, 600);
        let upd = debouncer.on_resize(zoomed, t0 + Duration::from_millis(250));
        assert!(upd.media_rescale_pending);
        let settled = debouncer.poll_settled(t0 + Duration::from_millis(350));
        assert_eq!(settled.map(|s| s.geometry), Some(zoomed));
    }

    #[test]
    fn adaptive_frame_pacer_enforces_12fps_kitty_6fps_sixel_and_ssh_and_0fps_texttag() {
        let kitty_local = FramePacer::for_tier(GraphicsTier::KittyPlaceholder, false);
        assert_eq!(kitty_local.max_fps(), 12);
        assert!(kitty_local.is_enabled());

        let kitty_direct_local = FramePacer::for_tier(GraphicsTier::KittyDirect, false);
        assert_eq!(kitty_direct_local.max_fps(), 12);

        let kitty_ssh = FramePacer::for_tier(GraphicsTier::KittyPlaceholder, true);
        assert_eq!(kitty_ssh.max_fps(), 6);

        let sixel_local = FramePacer::for_tier(GraphicsTier::Sixel, false);
        assert_eq!(sixel_local.max_fps(), 6);

        let halfblock_ssh = FramePacer::for_tier(GraphicsTier::HalfBlock, true);
        assert_eq!(halfblock_ssh.max_fps(), 6);

        let text_tag = FramePacer::for_tier(GraphicsTier::TextTagOnly, false);
        assert_eq!(text_tag.max_fps(), 0);
        assert!(!text_tag.is_enabled());
        assert_eq!(text_tag.min_frame_interval(), None);

        // Dynamic throttle halves 12 fps -> 6 fps cleanly.
        let mut throttled = kitty_local;
        throttled.set_throttle_divisor(2);
        assert_eq!(throttled.max_fps(), 6);
    }

    #[test]
    fn gif_animation_player_respects_pacer_cap_intrinsic_delay_and_focus() {
        let t0 = Instant::now();
        // Fast 20 ms/frame (50 fps) GIF must be paced down to 12 fps (~83.3 ms) on local Kitty
        // and 6 fps (~166.7 ms) on Sixel/SSH!
        let fast_gif = make_animated_media(4, 20);
        let pacer_ssh = FramePacer::for_tier(GraphicsTier::Sixel, true);
        let mut player = GifAnimationPlayer::new(fast_gif, true);

        // First tick anchors timestamp at frame 0.
        assert_eq!(player.tick(t0, &pacer_ssh), None);
        assert_eq!(player.frame_index(), 0);

        // At +100 ms (< 166.7 ms), must NOT advance yet!
        assert_eq!(player.tick(t0 + Duration::from_millis(100), &pacer_ssh), None);
        assert_eq!(player.frame_index(), 0);

        // At +170 ms (>= 166.7 ms), advances cleanly to frame 1.
        assert_eq!(
            player.tick(t0 + Duration::from_millis(170), &pacer_ssh),
            Some(1)
        );
        assert_eq!(player.frame_index(), 1);

        // Unfocusing resets cleanly to frame 0 and emits no further ticks.
        player.set_focused(false);
        assert_eq!(player.frame_index(), 0);
        assert_eq!(player.tick(t0 + Duration::from_secs(5), &pacer_ssh), None);

        // Slow 400 ms/frame GIF on 12 fps Kitty must NOT speed up past its 400 ms intrinsic delay!
        let slow_gif = make_animated_media(3, 400);
        let pacer_kitty = FramePacer::for_tier(GraphicsTier::KittyPlaceholder, false);
        let mut slow_player = GifAnimationPlayer::new(slow_gif, true);
        assert_eq!(slow_player.tick(t0, &pacer_kitty), None);
        assert_eq!(
            slow_player.tick(t0 + Duration::from_millis(200), &pacer_kitty),
            None
        );
        assert_eq!(
            slow_player.tick(t0 + Duration::from_millis(400), &pacer_kitty),
            Some(1)
        );
    }

    #[test]
    fn scroll_proxy_debouncer_gates_non_placeholder_pixel_tiers_only() {
        let t0 = Instant::now();
        let mut proxy = ScrollProxyDebouncer::with_default_window();
        proxy.on_scroll(t0);

        assert!(proxy.should_use_proxy(GraphicsTier::Sixel, t0 + Duration::from_millis(50)));
        assert!(proxy.should_use_proxy(GraphicsTier::ITerm2, t0 + Duration::from_millis(50)));
        assert!(proxy.should_use_proxy(GraphicsTier::KittyDirect, t0 + Duration::from_millis(50)));
        // KittyPlaceholder scrolls natively as Unicode text and never needs a scroll proxy.
        assert!(!proxy.should_use_proxy(
            GraphicsTier::KittyPlaceholder,
            t0 + Duration::from_millis(50)
        ));

        // After 150 ms settle window, full pixel rendering resumes.
        assert!(!proxy.should_use_proxy(GraphicsTier::Sixel, t0 + Duration::from_millis(150)));
    }
}
