//! Integrated TUI runtime loop (`run_interactive`) and 10-frame E2E verifier (`run_headless_smoke`).

use std::collections::{HashMap, HashSet};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bbs_core::{
    BackendEvent, CharsetMode, GraphicsTier, OsKeyringStore, ResolvedCharset, RgbaFrame, UiAction,
    VideoSinkKind, ViewportGeometry, mock_ids,
};
use bbs_runtime_term::{
    DecodedMedia, FramePacer, GifAnimationPlayer, MediaDecoderPool, MediaEncodeRequest,
    MediaEncoderPool, MediaFitMode, MediaRenderer, MediaWidget, ProbeOptions, ProbeReport,
    PtyBandwidthGovernor, RenderedMedia, RenderedMediaFrame, ResizeDebouncer,
    ScrollProxyDebouncer, TerminalEnv, TerminalSignal, TtyGuard, TtyGuardConfig,
    install_panic_hook, negotiate_capabilities, normalize_key_event, probe_terminal_async,
    query_stdout_winsize,
};
use bbs_tui::theme::{ThemeCatalog, ThemeDirs};
use bbs_tui::{App, Effect, ImageSlot, Mode, TuiConfig};
use bbs_voice::InputMode;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use tracing::warn;

use crate::bridge::{RuntimeServices, default_fingerprint};
use crate::cli::Cli;
use crate::config::{AppConfig, OptionalApiSecrets, save_config};
use crate::login::{ensure_interactive_tty, run_interactive_login, try_resolve_stored_token};
use crate::platform::{
    copy_to_clipboard, detect_local_utc_offset_minutes, open_external_url, run_external_editor,
};

/// Negative-cache cooldown preventing failed/404/unsupported media URLs from triggering 10 Hz
/// re-fetch storms.
const FAILED_URL_COOLDOWN: Duration = Duration::from_secs(60);

/// Upper bound on cached encoded frames kept in memory by [`MediaCoordinator`].
const MAX_ENCODED_FRAMES: usize = 256;

type EncodedCacheKey = (String, usize, u16, u16, GraphicsTier);
type DecodedOutcome = (String, Option<Arc<DecodedMedia>>, Vec<String>);

struct CachedEncodedFrame {
    media: RenderedMedia,
    is_animated: bool,
    emitted: bool,
}

/// Media pipeline state pairing `bbs-tui`'s [`ImageSlot`]s with `bbs-runtime-term`'s
/// `MediaRenderer`, `MediaDecoderPool`, off-thread `MediaEncoderPool`, `GifAnimationPlayer`,
/// `ScrollProxyDebouncer`, and `PtyBandwidthGovernor`.
struct MediaCoordinator {
    renderer: MediaRenderer,
    encoder: MediaEncoderPool,
    encoded_rx: mpsc::UnboundedReceiver<RenderedMediaFrame>,
    pool: Arc<MediaDecoderPool>,
    pacer: FramePacer,
    governor: PtyBandwidthGovernor,
    scroll_proxy: ScrollProxyDebouncer,
    players: HashMap<String, GifAnimationPlayer>,
    encoded: HashMap<EncodedCacheKey, CachedEncodedFrame>,
    in_flight: HashSet<String>,
    failed_urls: HashMap<String, Instant>,
    decoded_tx: mpsc::UnboundedSender<DecodedOutcome>,
    decoded_rx: mpsc::UnboundedReceiver<DecodedOutcome>,
    last_slot_geom: Vec<(String, Rect, Rect)>,
    cell_px: (u16, u16),
    charset: ResolvedCharset,
    tmux_passthrough: bool,
}

impl MediaCoordinator {
    fn new(report: &ProbeReport) -> Self {
        let (decoded_tx, decoded_rx) = mpsc::unbounded_channel();
        let (encoder, encoded_rx) = MediaEncoderPool::from_probe(report);
        Self {
            renderer: MediaRenderer::from_probe(report),
            encoder,
            encoded_rx,
            pool: Arc::new(MediaDecoderPool::with_defaults()),
            pacer: FramePacer::from_capabilities(&report.capabilities),
            governor: PtyBandwidthGovernor::for_session(report.capabilities.over_ssh),
            scroll_proxy: ScrollProxyDebouncer::with_default_window(),
            players: HashMap::new(),
            encoded: HashMap::new(),
            in_flight: HashSet::new(),
            failed_urls: HashMap::new(),
            decoded_tx,
            decoded_rx,
            last_slot_geom: Vec::new(),
            cell_px: (report.cell_pixels.0.max(1), report.cell_pixels.1.max(1)),
            charset: report.resolved_charset,
            tmux_passthrough: report.tmux_passthrough_active,
        }
    }

    fn uses_halfblock_images(&self) -> bool {
        self.renderer.tier() == GraphicsTier::HalfBlock
    }

    fn set_cell_pixels(&mut self, cell_px: (u16, u16)) {
        self.cell_px = (cell_px.0.max(1), cell_px.1.max(1));
        self.renderer.set_cell_pixels(self.cell_px);
    }

    fn clear_encoded(&mut self) {
        self.encoded.clear();
        self.encoder.clear_in_flight();
        self.in_flight.clear();
    }

    fn scroll_settle_deadline(&self, now: Instant) -> Option<Instant> {
        if self.scroll_proxy.should_use_proxy(self.renderer.tier(), now) {
            self.scroll_proxy.settle_deadline()
        } else {
            None
        }
    }

    fn evict_keys(&mut self, evicted: &[String], app: &mut App) {
        for key in evicted {
            app.images.remove(key);
            self.players.remove(key);
            self.encoded.retain(|(k, _, _, _, _), _| k != key);
        }
    }

    fn ingest_video_frame(&mut self, frame: &bbs_core::VideoFrame, app: &mut App) {
        let rgba = RgbaFrame {
            width: frame.width,
            height: frame.height,
            delay_ms: 100,
            pixels: frame.rgba.clone(),
        };
        // Key MUST match raw `frame.stream_key` emitted by `bbs-tui/src/panes.rs` Stage `ImageSlot`.
        let key = frame.stream_key.clone();
        if self.uses_halfblock_images() {
            app.images.insert(key.clone(), rgba.clone());
        }
        let decoded = DecodedMedia::from_rgba_frames(
            key.clone(),
            Some(format!("{}.rgba", frame.stream_key)),
            frame.width,
            frame.height,
            vec![rgba],
        );
        let outcome = self.pool.insert_cached(Arc::new(decoded));
        self.evict_keys(&outcome.evicted_keys, app);
        self.encoded.retain(|(k, _, _, _, _), _| k != &key);
    }

    fn apply_decoded_outcome(&mut self, outcome: DecodedOutcome, app: &mut App) -> bool {
        let (key, maybe_media, evicted) = outcome;
        self.in_flight.remove(&key);
        self.evict_keys(&evicted, app);

        let Some(media) = maybe_media else {
            self.failed_urls.insert(key, Instant::now());
            return false;
        };
        self.failed_urls.remove(&key);
        if self.uses_halfblock_images()
            && let Some(first) = media.frame(0)
        {
            app.images.insert(key.clone(), first.clone());
        }
        if media.is_animated {
            self.players.insert(
                key.clone(),
                GifAnimationPlayer::new(Arc::clone(&media), false),
            );
        } else {
            self.players.remove(&key);
        }
        let insert_outcome = self.pool.insert_cached(media);
        self.evict_keys(&insert_outcome.evicted_keys, app);
        self.encoded.retain(|(k, _, _, _, _), _| k != &key);
        true
    }

    fn ingest_decoded(&mut self, app: &mut App) -> bool {
        let mut changed = false;
        while let Ok(outcome) = self.decoded_rx.try_recv() {
            if self.apply_decoded_outcome(outcome, app) {
                changed = true;
            }
        }
        changed
    }

    fn apply_encoded_frame(&mut self, frame: RenderedMediaFrame) {
        let Some((url, idx_str)) = frame.key.rsplit_once('\0') else {
            return;
        };
        let Ok(frame_idx) = idx_str.parse::<usize>() else {
            return;
        };
        if self.encoded.len() >= MAX_ENCODED_FRAMES {
            self.encoded.clear();
        }
        let cache_key = (
            url.to_string(),
            frame_idx,
            frame.width_cells,
            frame.height_cells,
            frame.tier,
        );
        // Cache BEFORE checking `PtyBandwidthGovernor` so throttled frames are never re-encoded at 10 Hz.
        self.encoded.insert(
            cache_key,
            CachedEncodedFrame {
                media: frame.media,
                is_animated: frame.is_animated,
                emitted: false,
            },
        );
    }

    fn ingest_encoded(&mut self) -> bool {
        let mut changed = false;
        while let Ok(frame) = self.encoded_rx.try_recv() {
            self.apply_encoded_frame(frame);
            changed = true;
        }
        changed
    }

    fn sync_mock_gif_frames(&mut self, page: &bbs_core::GifSearchPage, app: &mut App) {
        for item in &page.items {
            if let Some(ref frames) = item.preview_rgba_frames
                && !frames.is_empty()
            {
                if self.uses_halfblock_images() {
                    app.images.insert(item.preview_url.clone(), frames[0].clone());
                }
                let decoded = DecodedMedia::from_rgba_frames(
                    item.preview_url.clone(),
                    Some(format!("{}.gif", item.id)),
                    frames[0].width,
                    frames[0].height,
                    frames.clone(),
                );
                let arc = Arc::new(decoded);
                let outcome = self.pool.insert_cached(Arc::clone(&arc));
                self.evict_keys(&outcome.evicted_keys, app);
                if arc.is_animated {
                    self.players.insert(
                        item.preview_url.clone(),
                        GifAnimationPlayer::new(arc, false),
                    );
                }
            }
        }
    }

    fn request_missing_slots(
        &mut self,
        slots: &[ImageSlot],
        services: &RuntimeServices,
        app: &mut App,
    ) {
        let now = Instant::now();
        self.failed_urls
            .retain(|_, failed_at| now.saturating_duration_since(*failed_at) < FAILED_URL_COOLDOWN);

        for slot in slots {
            // Inline video stream keys (e.g. `guild:...` / `call:...`) are populated directly by
            // `ingest_video_frame` and must never be HTTP-fetched or replaced with mock previews.
            if !slot.url.starts_with("http://") && !slot.url.starts_with("https://") {
                continue;
            }
            if self.pool.get_cached(&slot.url).is_some()
                || self.in_flight.contains(&slot.url)
                || self.failed_urls.contains_key(&slot.url)
            {
                continue;
            }
            self.in_flight.insert(slot.url.clone());

            if let Some(ref rest) = services.rest_client {
                let rest = Arc::clone(rest);
                let pool = Arc::clone(&self.pool);
                let tx = self.decoded_tx.clone();
                let url = slot.url.clone();
                tokio::spawn(async move {
                    let mut result: Option<Arc<DecodedMedia>> = None;
                    let mut evicted: Vec<String> = Vec::new();
                    if let Ok((bytes, _)) = rest.fetch_attachment_bytes(&url).await {
                        let name = url
                            .rsplit('/')
                            .next()
                            .unwrap_or("image.bin")
                            .split('?')
                            .next()
                            .unwrap_or("image.bin")
                            .to_string();
                        if let Ok((decoded, ev)) =
                            pool.decode_and_cache(url.clone(), bytes, Some(name)).await
                        {
                            result = Some(decoded);
                            evicted = ev;
                        }
                    }
                    // Always notify completion so failed fetches release `in_flight` and enter cooldown.
                    let _ = tx.send((url, result, evicted));
                });
            } else {
                // Offline mock mode: synthesize a crisp deterministic preview frame so pixel tiers render offline.
                let frame = synthesize_mock_preview_frame(&slot.url, 32, 24);
                if self.uses_halfblock_images() {
                    app.images.insert(slot.url.clone(), frame.clone());
                }
                let decoded = DecodedMedia::from_rgba_frames(
                    slot.url.clone(),
                    Some("mock-preview.png".to_string()),
                    frame.width,
                    frame.height,
                    vec![frame],
                );
                let outcome = self.pool.insert_cached(Arc::new(decoded));
                self.evict_keys(&outcome.evicted_keys, app);
                self.in_flight.remove(&slot.url);
            }
        }
    }

    fn tick_animations(&mut self, slots: &[ImageSlot], app: &mut App, now: Instant) -> bool {
        let mut any_changed = false;
        let halfblock = self.uses_halfblock_images();
        for slot in slots {
            // Tick ANY visible slot that decoded with animation frames (GIF or animated WebP),
            // even if the slot's URL lacked a `.gif` suffix.
            if let Some(player) = self.players.get_mut(&slot.url) {
                player.set_focused(slot.focused);
                if player.tick(now, &self.pacer).is_some() {
                    if halfblock && let Some(frame) = player.current_frame() {
                        app.images.insert(slot.url.clone(), frame.clone());
                    }
                    any_changed = true;
                }
            }
        }
        any_changed
    }

    fn overlay_pixel_slots(
        &mut self,
        slots: &[ImageSlot],
        buf: &mut ratatui::buffer::Buffer,
        is_resizing: bool,
        now: Instant,
    ) {
        if matches!(
            self.renderer.tier(),
            GraphicsTier::HalfBlock | GraphicsTier::TextTagOnly
        ) {
            return;
        }

        // Compare ONLY geometry `(url, rect, visible)` — NOT `focused` — so moving selection
        // never triggers a spurious 150 ms HalfBlock scroll-proxy flash.
        let current_geom: Vec<(String, Rect, Rect)> = slots
            .iter()
            .map(|s| (s.url.clone(), s.rect, s.visible))
            .collect();
        if current_geom != self.last_slot_geom {
            self.scroll_proxy.on_scroll(now);
            self.last_slot_geom = current_geom;
        }
        let use_proxy = is_resizing
            || self
                .scroll_proxy
                .should_use_proxy(self.renderer.tier(), now);
        let target_tier = if use_proxy {
            GraphicsTier::HalfBlock
        } else {
            self.renderer.tier()
        };

        for slot in slots {
            if slot.visible.width == 0 || slot.visible.height == 0 {
                continue;
            }
            let Some(media) = self.pool.get_cached(&slot.url) else {
                continue;
            };
            let frame_idx = self
                .players
                .get(&slot.url)
                .map_or(0, GifAnimationPlayer::frame_index);
            let key: EncodedCacheKey = (
                slot.url.clone(),
                frame_idx,
                slot.visible.width,
                slot.visible.height,
                target_tier,
            );

            if let Some(entry) = self.encoded.get_mut(&key) {
                if !entry.emitted {
                    let est = entry.media.estimated_pty_bytes();
                    if self.governor.can_emit_for(est, entry.is_animated, now) {
                        self.governor.record_emitted(est, now);
                        entry.emitted = true;
                    }
                }
                if entry.emitted {
                    let widget = MediaWidget::new(&entry.media);
                    ratatui::widgets::Widget::render(widget, slot.visible, buf);
                    continue;
                }
            } else if let Some(rgba) = media.frame(frame_idx) {
                // Enqueue off-main-thread encoding via `MediaEncoderPool` (deduplicated per
                // `(job_key, width, height, tier)` so `terminal.draw` never stalls).
                let stable_job_key = format!("{}\0{frame_idx}", slot.url);
                let mut req = MediaEncodeRequest::new(
                    stable_job_key,
                    Arc::new(rgba.clone()),
                    slot.visible.width,
                    slot.visible.height,
                    target_tier,
                    self.cell_px,
                );
                req.fit = MediaFitMode::Contain;
                req.charset = self.charset;
                req.tmux_passthrough = self.tmux_passthrough;
                req.title = media.title.clone();
                req.is_animated = media.is_animated;

                let _ = self.encoder.enqueue_request(req);
            }

            // While the requested frame is encoding off-thread or waiting for the animation
            // bandwidth budget, keep drawing the latest already-emitted frame for this slot size
            // to avoid flicker.
            if let Some((_, fallback)) = self.encoded.iter().find(|((u, _, w, h, _), v)| {
                u == &slot.url && *w == slot.visible.width && *h == slot.visible.height && v.emitted
            }) {
                let widget = MediaWidget::new(&fallback.media);
                ratatui::widgets::Widget::render(widget, slot.visible, buf);
            }
        }
    }
}

fn synthesize_mock_preview_frame(seed_str: &str, width: u32, height: u32) -> RgbaFrame {
    let seed: u32 = seed_str
        .bytes()
        .fold(17u32, |acc, b| acc.wrapping_mul(31).wrapping_add(u32::from(b)));
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let r = ((x * 8 + (seed & 0xFF)) % 256) as u8;
            let g = ((y * 10 + ((seed >> 8) & 0xFF)) % 256) as u8;
            let b = (((x + y) * 6 + ((seed >> 16) & 0xFF)) % 256) as u8;
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    RgbaFrame {
        width,
        height,
        delay_ms: 120,
        pixels,
    }
}

/// Build [`ProbeOptions`] by merging CLI overrides with persisted [`AppConfig`].
#[must_use]
pub fn build_probe_options(cli: &Cli, app_cfg: &AppConfig) -> ProbeOptions {
    let charset_override = cli
        .charset
        .map(CharsetMode::from)
        .or_else(|| app_cfg.parsed_charset())
        .unwrap_or(CharsetMode::Auto);
    let graphics_override = cli
        .graphics
        .and_then(|g| g.to_tier_override())
        .or_else(|| app_cfg.parsed_graphics());
    let color_depth_override = cli
        .color
        .and_then(|c| c.to_depth_override())
        .or_else(|| app_cfg.parsed_color_depth());

    ProbeOptions {
        geometry_override: cli.geometry,
        charset_override,
        graphics_override,
        color_depth_override,
        tmux_passthrough: cli.tmux_passthrough,
        skip_active_probe: cli.no_probe || cli.headless_smoke,
        ..ProbeOptions::default()
    }
}

/// Build resolved [`TuiConfig`] from a [`ProbeReport`], [`TerminalEnv`], [`AppConfig`], and [`Cli`].
#[must_use]
pub fn build_tui_config(
    report: &ProbeReport,
    env: &TerminalEnv,
    app_cfg: &AppConfig,
    cli: &Cli,
) -> TuiConfig {
    let mut cfg = TuiConfig::from_capabilities(
        &report.capabilities,
        env.locale_is_utf8(),
        env.term.as_deref(),
    );

    if let Some(charset_arg) = cli.charset {
        cfg.charset = CharsetMode::from(charset_arg)
            .resolve_with_vt(env.locale_is_utf8(), env.is_linux_vt());
    } else if report.vt_safe_charset && app_cfg.parsed_charset().is_none() {
        cfg.charset = ResolvedCharset::VtSafe;
    }

    if let Some(graphics) = cli
        .graphics
        .and_then(|g| g.to_tier_override())
        .or_else(|| app_cfg.parsed_graphics())
    {
        cfg.graphics = graphics;
    }
    if cfg.charset == ResolvedCharset::Ascii7Bit {
        cfg.graphics = GraphicsTier::TextTagOnly;
    }
    if let Some(depth) = cli
        .color
        .and_then(|c| c.to_depth_override())
        .or_else(|| app_cfg.parsed_color_depth())
    {
        cfg.depth = depth;
    }

    cfg.lcd_a02 = cli.lcd_a02 || app_cfg.lcd_a02;
    cfg.frame = app_cfg.parsed_frame_style();
    cfg.gradient_borders = app_cfg.gradient_borders && !cfg.low_bandwidth;
    cfg.ctrl_k = app_cfg.ctrl_k;
    cfg.max_decks = app_cfg.max_decks.clamp(1, 3);
    cfg.prefer_inline_video = app_cfg.prefer_inline_video;
    cfg.inline_video_low_tier = app_cfg.inline_video_low_tier;
    cfg.inline_video_ssh = app_cfg.inline_video_ssh;
    cfg.cell_px = report.cell_pixels;
    cfg.theme = cli.theme.clone().unwrap_or_else(|| app_cfg.theme.clone());
    cfg
}

/// Apply the requested theme (by exact label or slug) from the discovered [`ThemeCatalog`].
fn apply_initial_theme(app: &mut App, catalog: &ThemeCatalog, requested: &str) {
    if let Some(idx) = catalog.position(requested)
        && let Ok(spec) = catalog.load(idx)
    {
        app.cfg.theme = spec.name.clone();
        app.set_theme(spec);
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Run the full integrated interactive TUI runtime (`--mock-demo` or live Discord mode).
pub async fn run_interactive(
    cli: &Cli,
    config_path: &Path,
    mut app_cfg: AppConfig,
    api_secrets: OptionalApiSecrets,
) -> Result<()> {
    ensure_interactive_tty()?;

    // 1. Install panic hook BEFORE touching raw mode or alternate screen.
    install_panic_hook();

    // 2. Startup capability & geometry probe.
    let env = TerminalEnv::from_process();
    let probe_opts = build_probe_options(cli, &app_cfg);
    let probe_report = probe_terminal_async(probe_opts)
        .await
        .context("terminal capability probe failed")?;

    let tui_cfg = build_tui_config(&probe_report, &env, &app_cfg, cli);

    // 3. Enter RAII TtyGuard and subscribe to POSIX signals (`SIGWINCH`, `SIGCONT`, `SIGTERM`, `SIGHUP`).
    let mut guard_cfg = TtyGuardConfig::from_probe(&probe_report);
    guard_cfg.enable_mouse = guard_cfg.enable_mouse
        && tui_cfg.charset != ResolvedCharset::Ascii7Bit
        && probe_report.capabilities.geometry.cols >= 24
        && probe_report.capabilities.geometry.rows > 4;
    let mut tty_guard =
        TtyGuard::enter(guard_cfg).context("failed to initialize raw terminal guard")?;
    #[cfg(unix)]
    let mut signal_rx = bbs_runtime_term::spawn_signal_listener()
        .context("failed to start POSIX signal listener")?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to construct Ratatui terminal")?;
    terminal.clear()?;

    // 4. Construct either mock or live services (running interactive QR/Token login if needed).
    let (mut services, mut backend_rx) = if cli.mock_demo {
        let pair = RuntimeServices::build_mock();
        pair.0.seed_mock_state().await?;
        pair
    } else {
        let keyring = OsKeyringStore::new();
        let fingerprint = default_fingerprint()?;
        let auth = match try_resolve_stored_token(&keyring, cli.import_discordo_keyring)? {
            Some(resolved) => resolved,
            None => {
                let Some(logged_in) = run_interactive_login(
                    &mut terminal,
                    fingerprint.clone(),
                    tui_cfg.charset,
                    &keyring,
                )
                .await?
                else {
                    return Ok(());
                };
                logged_in
            }
        };
        tracing::info!(origin = ?auth.origin, "starting live Discord session");
        terminal.clear()?;
        RuntimeServices::build_live(auth.token, &app_cfg, &api_secrets, fingerprint).await?
    };

    // 5. Initialize TUI model + theme catalog + media coordinator + 100 ms resize debouncer.
    let theme_dirs = ThemeDirs::from_env();
    let catalog = ThemeCatalog::discover(&theme_dirs);
    let mut app = App::new(tui_cfg.clone());
    app.utc_offset_min = detect_local_utc_offset_minutes();
    app.show_sidebar = app_cfg.show_sidebar;
    app.show_members = app_cfg.show_members;
    app.theme_dirs = theme_dirs;
    apply_initial_theme(&mut app, &catalog, &tui_cfg.theme);

    let mut persisted_theme = app.theme().name().to_string();
    let mut media = MediaCoordinator::new(&probe_report);
    let mut debouncer = ResizeDebouncer::with_default_window(probe_report.capabilities.geometry);
    let sync_output = probe_report.capabilities.synchronized_output;
    let osc52 = probe_report.capabilities.osc52_clipboard;
    let mut term_events = EventStream::new();
    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));
    let mut tick_count = 0u64;
    let mut ptt_held = false;
    let mut should_quit = false;

    while !should_quit {
        let now = Instant::now();
        media.ingest_decoded(&mut app);
        media.ingest_encoded();

        // Emit frame wrapped in DEC Mode 2026 synchronized output when supported.
        if sync_output {
            let mut out = io::stdout();
            let _ = out.write_all(bbs_runtime_term::tty_guard::BEGIN_SYNC_OUTPUT_SEQ);
        }

        terminal.draw(|frame| {
            let area = frame.area();
            let cursor = app.render_buffer(area, frame.buffer_mut());
            let slots = app.image_slots().to_vec();
            media.overlay_pixel_slots(&slots, frame.buffer_mut(), debouncer.is_debouncing(), now);
            if let Some((x, y)) = cursor {
                frame.set_cursor_position(ratatui::layout::Position::new(x, y));
            }
        })?;

        if sync_output {
            let mut out = io::stdout();
            let _ = out.write_all(bbs_runtime_term::tty_guard::END_SYNC_OUTPUT_SEQ);
            let _ = out.flush();
        }

        let current_slots = app.image_slots().to_vec();
        media.request_missing_slots(&current_slots, &services, &mut app);

        // Persist theme changes committed via the F2 / Ctrl+T theme picker.
        if app.theme().name() != persisted_theme && app.cfg.theme == app.theme().name() {
            persisted_theme = app.theme().name().to_string();
            app_cfg.theme = persisted_theme.clone();
            if let Err(err) = save_config(config_path, &app_cfg) {
                warn!(error = %err, "failed to persist theme change to config.toml");
            }
        }

        let mut effects: Vec<Effect> = Vec::new();
        let debounce_deadline = debouncer.next_deadline();
        let debounce_sleep = async move {
            match debounce_deadline {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let scroll_settle_deadline = media.scroll_settle_deadline(Instant::now());
        let scroll_settle_sleep = async move {
            match scroll_settle_deadline {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                }
                None => std::future::pending::<()>().await,
            }
        };

        #[cfg(unix)]
        tokio::select! {
            biased;
            maybe_sig = signal_rx.recv() => {
                match maybe_sig {
                    Some(TerminalSignal::Resize(geom)) => {
                        if let Some(px) = geom.cell_pixels() {
                            app.cfg.cell_px = px;
                            media.set_cell_pixels(px);
                        }
                        debouncer.on_resize_now(geom);
                        effects.extend(app.handle_resize(geom.cols, geom.rows));
                    }
                    Some(TerminalSignal::Continued(geom)) => {
                        if let Ok(resume) = tty_guard.resume_after_sigcont() {
                            debouncer.commit_immediate(resume.geometry);
                            media.clear_encoded();
                            let _ = terminal.clear();
                            effects.extend(app.handle_resize(resume.geometry.cols, resume.geometry.rows));
                        } else {
                            debouncer.commit_immediate(geom);
                            let _ = terminal.clear();
                        }
                    }
                    Some(TerminalSignal::Terminate) => {
                        should_quit = true;
                    }
                    None => {}
                }
            }
            maybe_term = term_events.next() => {
                match maybe_term {
                    Some(Ok(Event::Key(raw_key))) => {
                        handle_interactive_key(
                            raw_key,
                            &mut app,
                            &app_cfg,
                            &services,
                            &mut ptt_held,
                            osc52,
                            &mut effects,
                        );
                    }
                    Some(Ok(Event::Mouse(mouse_ev))) => {
                        effects.extend(app.handle_mouse(mouse_ev));
                    }
                    Some(Ok(Event::Paste(text))) => {
                        effects.extend(app.handle_paste(&text));
                    }
                    Some(Ok(Event::FocusLost)) => {
                        if ptt_held {
                            ptt_held = false;
                            services.set_ptt_active(false);
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        let geom = query_stdout_winsize()
                            .unwrap_or_else(|| ViewportGeometry::new(cols, rows));
                        if let Some(px) = geom.cell_pixels() {
                            app.cfg.cell_px = px;
                            media.set_cell_pixels(px);
                        }
                        debouncer.on_resize_now(geom);
                        effects.extend(app.handle_resize(cols, rows));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        should_quit = true;
                    }
                }
            }
            maybe_ev = backend_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    match &ev {
                        BackendEvent::VideoFrameReceived(frame) => {
                            media.ingest_video_frame(frame, &mut app);
                        }
                        BackendEvent::GifSearchResults { page, .. } => {
                            media.sync_mock_gif_frames(page, &mut app);
                        }
                        _ => {}
                    }
                    effects.extend(app.handle_event(ev));
                }
            }
            maybe_decoded = media.decoded_rx.recv() => {
                if let Some(outcome) = maybe_decoded {
                    let _ = media.apply_decoded_outcome(outcome, &mut app);
                }
            }
            maybe_encoded = media.encoded_rx.recv() => {
                if let Some(frame) = maybe_encoded {
                    media.apply_encoded_frame(frame);
                }
            }
            () = debounce_sleep => {
                if let Some(settled) = debouncer.poll_settled_now() {
                    if let Some(px) = settled.geometry.cell_pixels() {
                        app.cfg.cell_px = px;
                        media.set_cell_pixels(px);
                    }
                    media.clear_encoded();
                }
            }
            () = scroll_settle_sleep => {}
            _ = tick_interval.tick() => {
                tick_count = tick_count.saturating_add(1);
                let now_tick = Instant::now();
                effects.extend(app.tick(current_unix_ms()));
                let slots = app.image_slots().to_vec();
                let _ = media.tick_animations(&slots, &mut app, now_tick);

                // In --mock-demo, animate deterministic voice VU levels and SMPTE test cards.
                if let Some(ref mv) = services.mock_voice {
                    let _ = mv.step_audio_levels(tick_count);
                }
                if tick_count.is_multiple_of(3)
                    && let Some(ref mvid) = services.mock_video
                {
                    for key in [mock_ids::STREAM_RETRONET, mock_ids::STREAM_GROUP_DM] {
                        if mvid.active_sink(key).ok().flatten() == Some(VideoSinkKind::InlineTerminal) {
                            let _ = mvid.emit_test_frame(key, 64, 36);
                        }
                    }
                }
            }
        }

        #[cfg(not(unix))]
        tokio::select! {
            biased;
            maybe_term = term_events.next() => {
                match maybe_term {
                    Some(Ok(Event::Key(raw_key))) => {
                        handle_interactive_key(
                            raw_key,
                            &mut app,
                            &app_cfg,
                            &services,
                            &mut ptt_held,
                            osc52,
                            &mut effects,
                        );
                    }
                    Some(Ok(Event::Mouse(mouse_ev))) => {
                        effects.extend(app.handle_mouse(mouse_ev));
                    }
                    Some(Ok(Event::Paste(text))) => {
                        effects.extend(app.handle_paste(&text));
                    }
                    Some(Ok(Event::FocusLost)) => {
                        if ptt_held {
                            ptt_held = false;
                            services.set_ptt_active(false);
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        let geom = ViewportGeometry::new(cols, rows);
                        debouncer.on_resize_now(geom);
                        effects.extend(app.handle_resize(cols, rows));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        should_quit = true;
                    }
                }
            }
            maybe_ev = backend_rx.recv() => {
                if let Some(ev) = maybe_ev {
                    match &ev {
                        BackendEvent::VideoFrameReceived(frame) => {
                            media.ingest_video_frame(frame, &mut app);
                        }
                        BackendEvent::GifSearchResults { page, .. } => {
                            media.sync_mock_gif_frames(page, &mut app);
                        }
                        _ => {}
                    }
                    effects.extend(app.handle_event(ev));
                }
            }
            maybe_decoded = media.decoded_rx.recv() => {
                if let Some(outcome) = maybe_decoded {
                    let _ = media.apply_decoded_outcome(outcome, &mut app);
                }
            }
            maybe_encoded = media.encoded_rx.recv() => {
                if let Some(frame) = maybe_encoded {
                    media.apply_encoded_frame(frame);
                }
            }
            () = debounce_sleep => {
                if debouncer.poll_settled_now().is_some() {
                    media.clear_encoded();
                }
            }
            () = scroll_settle_sleep => {}
            _ = tick_interval.tick() => {
                tick_count = tick_count.saturating_add(1);
                let now_tick = Instant::now();
                effects.extend(app.tick(current_unix_ms()));
                let slots = app.image_slots().to_vec();
                let _ = media.tick_animations(&slots, &mut app, now_tick);
            }
        }

        if ptt_held && app.store.voice.is_none() {
            ptt_held = false;
            services.set_ptt_active(false);
        }
        drain_platform_requests(&mut app, osc52);

        // Execute returned TUI Effects.
        for effect in effects {
            match effect {
                Effect::Action(UiAction::Quit) | Effect::Quit => {
                    should_quit = true;
                }
                Effect::Action(UiAction::SwitchTheme(name)) => {
                    let fresh_catalog = ThemeCatalog::discover(&app.theme_dirs);
                    if let Some(idx) = fresh_catalog.position(&name)
                        && let Ok(spec) = fresh_catalog.load(idx)
                    {
                        app.cfg.theme = spec.name.clone();
                        app.set_theme(spec);
                        persisted_theme = app.theme().name().to_string();
                        app_cfg.theme = persisted_theme.clone();
                        let _ = save_config(config_path, &app_cfg);
                    }
                }
                Effect::Action(action) => {
                    services.dispatch_action(action);
                }
                Effect::Redraw => {
                    let _ = tty_guard.refresh_modes();
                    media.clear_encoded();
                    let _ = terminal.clear();
                }
                Effect::Suspend => {
                    if let Ok(resume) = tty_guard.suspend() {
                        debouncer.commit_immediate(resume.geometry);
                        media.clear_encoded();
                        let _ = terminal.clear();
                        let _ = app.handle_resize(resume.geometry.cols, resume.geometry.rows);
                    }
                }
                Effect::ExternalEditor(initial) => {
                    if let Ok((maybe_edited, resume)) = run_external_editor(&mut tty_guard, &initial)
                    {
                        if let Some(edited) = maybe_edited {
                            app.set_composer_text(&edited);
                        }
                        debouncer.commit_immediate(resume.geometry);
                        media.clear_encoded();
                        let _ = terminal.clear();
                    }
                }
                Effect::SendAsFile {
                    channel_id,
                    content,
                    reply_to,
                } => {
                    services.dispatch_send_as_file(channel_id, content, reply_to);
                }
                Effect::Bell => {
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x07");
                    let _ = out.flush();
                }
            }
        }
    }

    services.shutdown().await;
    drop(terminal);
    let _ = tty_guard.restore();
    Ok(())
}

fn handle_interactive_key(
    raw_key: crossterm::event::KeyEvent,
    app: &mut App,
    app_cfg: &AppConfig,
    services: &RuntimeServices,
    ptt_held: &mut bool,
    osc52: bool,
    effects: &mut Vec<Effect>,
) {
    let key = normalize_key_event(raw_key);
    let ptt_enabled = app_cfg.voice.resolved_input_mode() == InputMode::PushToTalk;

    if ptt_enabled
        && matches_ptt_key(key.code, &app_cfg.voice.ptt_key)
        && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
    {
        if key.kind == KeyEventKind::Release {
            if *ptt_held {
                *ptt_held = false;
                services.set_ptt_active(false);
            }
            return;
        }
        let allow_intercept = matches!(key.code, KeyCode::F(_))
            || (app.mode == Mode::Normal && !app.has_modal());
        if allow_intercept && app.store.voice.is_some() {
            if !*ptt_held {
                *ptt_held = true;
                services.set_ptt_active(true);
            }
            return;
        }
    }

    // Optional Shift-free clipboard / URL convenience chords in NORMAL mode:
    if key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::ALT)
        && key.code == KeyCode::Char('y')
        && let Some(msg_text) = selected_message_text(app)
    {
        let _ = copy_to_clipboard(&msg_text, osc52);
    } else if key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::ALT)
        && key.code == KeyCode::Char('o')
        && let Some(url) = selected_message_url(app)
    {
        let _ = open_external_url(&url);
    } else {
        effects.extend(app.handle_key(key));
    }
}

fn drain_platform_requests(app: &mut App, osc52: bool) {
    while let Some(url) = app.take_open_url() {
        let _ = open_external_url(&url);
    }
    while let Some(text) = app.take_copy_text() {
        let _ = copy_to_clipboard(&text, osc52);
    }
}

fn matches_ptt_key(code: KeyCode, ptt_key: &str) -> bool {
    let spec = ptt_key.trim();
    if spec.is_empty() {
        return false;
    }
    if spec.eq_ignore_ascii_case("space") {
        return code == KeyCode::Char(' ');
    }
    if spec.eq_ignore_ascii_case("tab") {
        return code == KeyCode::Tab;
    }
    if spec.eq_ignore_ascii_case("enter") {
        return code == KeyCode::Enter;
    }
    if let Some(rest) = spec.strip_prefix('f').or_else(|| spec.strip_prefix('F'))
        && let Ok(n) = rest.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return code == KeyCode::F(n);
    }
    let mut chars = spec.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        return matches!(code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&ch));
    }
    false
}

fn selected_message_text(app: &App) -> Option<String> {
    let id = app.selected?;
    let msgs = app.active_messages()?;
    msgs.iter().find(|m| m.id == id).map(|m| m.content.clone())
}

fn selected_message_url(app: &App) -> Option<String> {
    let id = app.selected?;
    let msgs = app.active_messages()?;
    let msg = msgs.iter().find(|m| m.id == id)?;
    if let Some(url) = msg.embeds.iter().find_map(|e| e.url.clone()) {
        return Some(url);
    }
    if let Some(att) = msg.attachments.first() {
        return Some(att.url.clone());
    }
    msg.content
        .split_whitespace()
        .find(|w| w.starts_with("https://") || w.starts_with("http://"))
        .map(ToOwned::to_owned)
}

/// Run 10 full integrated render/update frames across `16×2`, `80×24`, and `512×144`
/// against the complete offline mock stack (`MockDiscordBackend`, `MockGifProvider`,
/// `MockVoiceEngine`, `MockVideoStreamEngine`), `ThemeCatalog`, and `MediaRenderer`, then exit `0`.
pub async fn run_headless_smoke(cli: &Cli, app_cfg: &AppConfig) -> Result<SmokeSummary> {
    let env = TerminalEnv::default();
    let probe_opts = ProbeOptions {
        geometry_override: Some(ViewportGeometry::new(80, 24)),
        charset_override: cli
            .charset
            .map(CharsetMode::from)
            .unwrap_or(CharsetMode::Utf8),
        graphics_override: cli
            .graphics
            .and_then(|g| g.to_tier_override())
            .or(Some(GraphicsTier::HalfBlock)),
        skip_active_probe: true,
        ..ProbeOptions::default()
    };
    let report = negotiate_capabilities(
        &env,
        ViewportGeometry::new(80, 24),
        bbs_runtime_term::GeometrySource::CliOverride,
        &bbs_runtime_term::ProbeResponse::default(),
        &probe_opts,
        false,
        true,
    );

    let tui_cfg = build_tui_config(&report, &env, app_cfg, cli);
    let catalog = ThemeCatalog::discover(&ThemeDirs::from_env());
    let mut app = App::new(tui_cfg.clone());
    app.cfg.prefer_inline_video = true;
    app.cfg.inline_video_low_tier = true;
    apply_initial_theme(&mut app, &catalog, &tui_cfg.theme);

    let (mut services, mut backend_rx) = RuntimeServices::build_mock();
    services.seed_mock_state().await?;

    let mut media = MediaCoordinator::new(&report);
    let mut debouncer = ResizeDebouncer::with_default_window(ViewportGeometry::new(80, 24));
    let mut frames_rendered = 0usize;
    let mut total_image_slots = 0usize;

    // Helper closure to drain pending backend events into `app`.
    let drain_events = |app: &mut App,
                        media: &mut MediaCoordinator,
                        rx: &mut mpsc::UnboundedReceiver<BackendEvent>| {
        while let Ok(ev) = rx.try_recv() {
            match &ev {
                BackendEvent::VideoFrameReceived(frame) => {
                    media.ingest_video_frame(frame, app);
                }
                BackendEvent::GifSearchResults { page, .. } => {
                    media.sync_mock_gif_frames(page, app);
                }
                _ => {}
            }
            let _ = app.handle_event(ev);
        }
    };

    // Frame plan: 10 integrated frames covering 16x2 (ASCII + UTF-8), 80x24, and 512x144.
    let stages: [(u16, u16, ResolvedCharset, &str); 10] = [
        (16, 2, ResolvedCharset::Ascii7Bit, "micro-16x2-ascii-read"),
        (16, 2, ResolvedCharset::Utf8, "micro-16x2-utf8-nav"),
        (16, 2, ResolvedCharset::Ascii7Bit, "micro-16x2-ascii-compose"),
        (80, 24, ResolvedCharset::Utf8, "standard-80x24-chat"),
        (80, 24, ResolvedCharset::Utf8, "standard-80x24-theme-switch"),
        (80, 24, ResolvedCharset::Utf8, "standard-80x24-voice-call"),
        (512, 144, ResolvedCharset::Utf8, "ultrawide-512x144-cockpit"),
        (512, 144, ResolvedCharset::Utf8, "ultrawide-512x144-inline-video"),
        (512, 144, ResolvedCharset::Utf8, "ultrawide-512x144-gif-picker"),
        (80, 24, ResolvedCharset::Utf8, "standard-80x24-reflow-restore"),
    ];

    for (idx, (cols, rows, charset, _label)) in stages.into_iter().enumerate() {
        let now = Instant::now();
        app.cfg.charset = charset;
        let geom = ViewportGeometry::new(cols, rows);
        debouncer.on_resize(geom, now);
        let _ = app.handle_resize(cols, rows);

        // Drive realistic cross-crate interactions across frames.
        match idx {
            0 => {
                app.select_channel(mock_ids::CH_RETRONET_GENERAL);
            }
            1 => {
                // Micro navigation key
                let _ = app.handle_key(crossterm::event::KeyEvent::new(
                    KeyCode::Down,
                    KeyModifiers::NONE,
                ));
            }
            3 => {
                // Send a message through MockDiscordBackend
                let _ = services
                    .discord
                    .send_message(mock_ids::CH_RETRONET_GENERAL, "smoke test ping", None)
                    .await?;
            }
            4 => {
                // Hot-switch theme from discovered ThemeCatalog
                if catalog.entries.len() > 1
                    && let Ok(spec) = catalog.load(1)
                {
                    app.cfg.theme = spec.name.clone();
                    app.set_theme(spec);
                }
            }
            5 => {
                // Join Group DM voice call (DAVE E2EE) + advance VU meter levels
                let _ = services
                    .voice
                    .join_voice(None, mock_ids::CH_GROUP_DM_OPS)
                    .await?;
                if let Some(ref mv) = services.mock_voice {
                    let _ = mv.step_audio_levels(5);
                }
            }
            6 => {
                // Watch inline SMPTE video stream in UltraWide Stage deck
                let _ = services
                    .video
                    .watch_stream(mock_ids::STREAM_GROUP_DM, VideoSinkKind::InlineTerminal)
                    .await?;
            }
            7 => {
                // Emit additional synthetic SMPTE frame & advance VU meters
                if let Some(ref mvid) = services.mock_video {
                    let _ = mvid.emit_test_frame(mock_ids::STREAM_GROUP_DM, 64, 36);
                }
                if let Some(ref mv) = services.mock_voice {
                    let _ = mv.step_audio_levels(8);
                }
            }
            8 => {
                // Search MockGifProvider and feed animated RGBA frames into MediaCoordinator
                let page = services.gifs.trending(6, None).await?;
                let _ = services.event_tx.send(BackendEvent::GifSearchResults {
                    request_id: 1,
                    page,
                });
            }
            9 => {
                // Settle resize debouncer cleanly
                let _ = debouncer.poll_settled(now + Duration::from_millis(150));
            }
            _ => {}
        }

        drain_events(&mut app, &mut media, &mut backend_rx);
        let _ = app.tick(1_700_000_000_000 + (idx as u64) * 150);
        media.ingest_decoded(&mut app);
        media.ingest_encoded();

        let mut terminal = Terminal::new(TestBackend::new(cols, rows))
            .context("failed to construct TestBackend for headless smoke")?;
        terminal.draw(|frame| {
            let area = Rect::new(0, 0, cols, rows);
            let _ = app.render_buffer(area, frame.buffer_mut());
            let slots = app.image_slots().to_vec();
            media.overlay_pixel_slots(&slots, frame.buffer_mut(), false, now);
        })?;

        let slots = app.image_slots().to_vec();
        media.request_missing_slots(&slots, &services, &mut app);
        total_image_slots = total_image_slots.saturating_add(slots.len());
        frames_rendered = frames_rendered.saturating_add(1);
    }

    services.shutdown().await;

    Ok(SmokeSummary {
        frames_rendered,
        themes_discovered: catalog.entries.len(),
        active_theme: app.theme().name().to_string(),
        total_image_slots,
    })
}

/// Summary metrics produced by [`run_headless_smoke`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeSummary {
    /// Total integrated frames rendered across `16×2`, `80×24`, and `512×144`.
    pub frames_rendered: usize,
    /// Number of themes discovered across built-ins, user dir, and host `btop` themes.
    pub themes_discovered: usize,
    /// Active theme name at end of smoke run.
    pub active_theme: String,
    /// Total [`ImageSlot`]s emitted across all rendered frames.
    pub total_image_slots: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbs_core::{MockVideoStreamEngine, VideoFrame};
    use clap::Parser as _;

    #[tokio::test]
    async fn headless_smoke_renders_ten_integrated_frames_cleanly() {
        let cli = Cli::try_parse_from(["bbscord", "--headless-smoke"]).unwrap();
        let summary = run_headless_smoke(&cli, &AppConfig::default())
            .await
            .unwrap();
        assert_eq!(summary.frames_rendered, 10);
        assert!(summary.themes_discovered >= 10);
    }

    #[tokio::test]
    async fn video_frame_key_matches_stream_slot_and_skips_http_fetch() {
        let env = TerminalEnv::default();
        let opts = ProbeOptions {
            charset_override: CharsetMode::Utf8,
            graphics_override: Some(GraphicsTier::HalfBlock),
            ..ProbeOptions::default()
        };
        let report = negotiate_capabilities(
            &env,
            ViewportGeometry::new(80, 24),
            bbs_runtime_term::GeometrySource::CliOverride,
            &bbs_runtime_term::ProbeResponse::default(),
            &opts,
            false,
            true,
        );
        let mut media = MediaCoordinator::new(&report);
        assert_eq!(media.renderer.tier(), GraphicsTier::HalfBlock);
        let mut app = App::new(TuiConfig::default());
        let (mut services, _rx) = RuntimeServices::build_mock();

        // Non-HTTP stream key slot before any frame arrives MUST NOT populate a fake preview or stick in_flight.
        let slot = ImageSlot {
            url: mock_ids::STREAM_GROUP_DM.to_string(),
            rect: Rect::new(0, 0, 20, 10),
            visible: Rect::new(0, 0, 20, 10),
            clip_top: 0,
            animated: true,
            focused: true,
        };
        media.request_missing_slots(std::slice::from_ref(&slot), &services, &mut app);
        assert!(media.pool.get_cached(mock_ids::STREAM_GROUP_DM).is_none());
        assert!(!media.in_flight.contains(mock_ids::STREAM_GROUP_DM));

        // Real VideoFrame ingested under raw `frame.stream_key` matches `ImageSlot.url`.
        let frame: VideoFrame =
            MockVideoStreamEngine::generate_test_card_frame(mock_ids::STREAM_GROUP_DM, 1, 32, 18);
        media.ingest_video_frame(&frame, &mut app);
        assert!(media.pool.get_cached(mock_ids::STREAM_GROUP_DM).is_some());
        assert!(app.images.contains_key(mock_ids::STREAM_GROUP_DM));

        // Failed async decode notifies `(url, None, evicted)`, clears `in_flight`, and enters 60s cooldown.
        let failed_url = "https://example.invalid/broken.png".to_string();
        media.in_flight.insert(failed_url.clone());
        media
            .decoded_tx
            .send((failed_url.clone(), None, Vec::new()))
            .unwrap();
        assert!(!media.ingest_decoded(&mut app));
        assert!(!media.in_flight.contains(&failed_url));
        assert!(media.failed_urls.contains_key(&failed_url));

        // Subsequent slot request during cooldown MUST NOT re-enter `in_flight`.
        let failed_slot = ImageSlot {
            url: failed_url.clone(),
            rect: Rect::new(0, 0, 10, 5),
            visible: Rect::new(0, 0, 10, 5),
            clip_top: 0,
            animated: false,
            focused: false,
        };
        media.request_missing_slots(&[failed_slot], &services, &mut app);
        assert!(!media.in_flight.contains(&failed_url));

        services.shutdown().await;
    }

    #[tokio::test]
    async fn kitty_tier_skips_halfblock_app_images_and_ignores_focus_changes_for_scroll_proxy() {
        let env = TerminalEnv::default();
        let opts = ProbeOptions {
            charset_override: CharsetMode::Utf8,
            graphics_override: Some(GraphicsTier::KittyPlaceholder),
            ..ProbeOptions::default()
        };
        let report = negotiate_capabilities(
            &env,
            ViewportGeometry::new(80, 24),
            bbs_runtime_term::GeometrySource::CliOverride,
            &bbs_runtime_term::ProbeResponse::default(),
            &opts,
            false,
            true,
        );
        let mut media = MediaCoordinator::new(&report);
        assert_eq!(media.renderer.tier(), GraphicsTier::KittyPlaceholder);
        let mut app = App::new(TuiConfig::default());

        let frame: VideoFrame =
            MockVideoStreamEngine::generate_test_card_frame(mock_ids::STREAM_GROUP_DM, 1, 32, 18);
        media.ingest_video_frame(&frame, &mut app);
        // Pixel tiers rely on MediaDecoderPool + MediaEncoderPool, not duplicated HalfBlock app.images.
        assert!(media.pool.get_cached(mock_ids::STREAM_GROUP_DM).is_some());
        assert!(!app.images.contains_key(mock_ids::STREAM_GROUP_DM));

        // First render establishes slot geometry; after settle, toggling `focused` must NOT re-trigger scroll proxy.
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 80, 24));
        let t0 = Instant::now();
        let slot_unfocused = ImageSlot {
            url: mock_ids::STREAM_GROUP_DM.to_string(),
            rect: Rect::new(2, 2, 20, 10),
            visible: Rect::new(2, 2, 20, 10),
            clip_top: 0,
            animated: true,
            focused: false,
        };
        media.overlay_pixel_slots(std::slice::from_ref(&slot_unfocused), &mut buf, false, t0);
        let t_settled = t0 + Duration::from_millis(500);
        assert!(media.scroll_settle_deadline(t_settled).is_none());

        let mut slot_focused = slot_unfocused;
        slot_focused.focused = true;
        media.overlay_pixel_slots(&[slot_focused], &mut buf, false, t_settled);
        assert!(media.scroll_settle_deadline(t_settled).is_none());
    }

    #[test]
    fn matches_ptt_key_handles_chars_fkeys_and_specials() {
        assert!(matches_ptt_key(KeyCode::Char('v'), "v"));
        assert!(matches_ptt_key(KeyCode::Char('V'), "v"));
        assert!(matches_ptt_key(KeyCode::Char(' '), "space"));
        assert!(matches_ptt_key(KeyCode::Tab, "TAB"));
        assert!(matches_ptt_key(KeyCode::F(12), "f12"));
        assert!(matches_ptt_key(KeyCode::F(4), "F4"));
        assert!(!matches_ptt_key(KeyCode::Char('x'), "v"));
        assert!(!matches_ptt_key(KeyCode::F(4), ""));
    }

    #[test]
    fn f2_theme_commit_updates_cfg_theme_for_persistence() {
        let mut app = App::new(TuiConfig::default());
        let initial = app.theme().name().to_string();
        assert_eq!(app.cfg.theme, initial);

        // Open theme switcher via F2, move down one entry, commit with Enter.
        let _ = app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::F(2),
            KeyModifiers::NONE,
        ));
        let _ = app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        // Hover preview MUST NOT mutate cfg.theme before Enter commit.
        assert_eq!(app.cfg.theme, initial);

        let _ = app.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert_ne!(app.theme().name(), initial);
        assert_eq!(app.cfg.theme, app.theme().name());
    }
}
