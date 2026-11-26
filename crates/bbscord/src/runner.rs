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
    DecodedMedia, FramePacer, GifAnimationPlayer, MediaDecoderPool, MediaFitMode, MediaRenderer,
    MediaWidget, ProbeOptions, ProbeReport, PtyBandwidthGovernor, RenderedMedia, ResizeDebouncer,
    ScrollProxyDebouncer, TerminalEnv, TerminalSignal, TtyGuard, TtyGuardConfig,
    install_panic_hook, negotiate_capabilities, normalize_key_event, probe_terminal_async,
    query_stdout_winsize,
};
use bbs_tui::theme::{ThemeCatalog, ThemeDirs};
use bbs_tui::{App, Effect, ImageSlot, TuiConfig};
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
use crate::platform::{copy_to_clipboard, open_external_url, run_external_editor};

/// Media pipeline state pairing `bbs-tui`'s [`ImageSlot`]s with `bbs-runtime-term`'s
/// `MediaRenderer`, `MediaDecoderPool`, `GifAnimationPlayer`, `ResizeDebouncer`, and `PtyBandwidthGovernor`.
struct MediaCoordinator {
    renderer: MediaRenderer,
    pool: Arc<MediaDecoderPool>,
    pacer: FramePacer,
    governor: PtyBandwidthGovernor,
    scroll_proxy: ScrollProxyDebouncer,
    players: HashMap<String, GifAnimationPlayer>,
    encoded: HashMap<(String, usize, u16, u16, bool), RenderedMedia>,
    in_flight: HashSet<String>,
    decoded_tx: mpsc::UnboundedSender<(String, Arc<DecodedMedia>)>,
    decoded_rx: mpsc::UnboundedReceiver<(String, Arc<DecodedMedia>)>,
    last_slots: Vec<ImageSlot>,
    dirty: bool,
}

impl MediaCoordinator {
    fn new(report: &ProbeReport) -> Self {
        let (decoded_tx, decoded_rx) = mpsc::unbounded_channel();
        Self {
            renderer: MediaRenderer::from_probe(report),
            pool: Arc::new(MediaDecoderPool::with_defaults()),
            pacer: FramePacer::from_capabilities(&report.capabilities),
            governor: PtyBandwidthGovernor::for_session(report.capabilities.over_ssh),
            scroll_proxy: ScrollProxyDebouncer::with_default_window(),
            players: HashMap::new(),
            encoded: HashMap::new(),
            in_flight: HashSet::new(),
            decoded_tx,
            decoded_rx,
            last_slots: Vec::new(),
            dirty: false,
        }
    }

    fn clear_encoded(&mut self) {
        self.encoded.clear();
        self.dirty = true;
    }

    fn ingest_video_frame(&mut self, frame: &bbs_core::VideoFrame, app: &mut App) {
        let rgba = RgbaFrame {
            width: frame.width,
            height: frame.height,
            delay_ms: 100,
            pixels: frame.rgba.clone(),
        };
        let key = format!("stream:{}", frame.stream_key);
        app.images.insert(key.clone(), rgba.clone());
        let decoded = DecodedMedia::from_rgba_frames(
            key.clone(),
            Some(format!("{}.rgba", frame.stream_key)),
            frame.width,
            frame.height,
            vec![rgba],
        );
        let _ = self.pool.insert_cached(Arc::new(decoded));
        self.encoded.retain(|(k, _, _, _, _), _| k != &key);
        self.dirty = true;
    }

    fn ingest_decoded(&mut self, app: &mut App) -> bool {
        let mut changed = false;
        while let Ok((key, media)) = self.decoded_rx.try_recv() {
            self.in_flight.remove(&key);
            if let Some(first) = media.frame(0) {
                app.images.insert(key.clone(), first.clone());
            }
            if media.is_animated {
                self.players.insert(
                    key.clone(),
                    GifAnimationPlayer::new(Arc::clone(&media), false),
                );
            }
            let _ = self.pool.insert_cached(media);
            self.encoded.retain(|(k, _, _, _, _), _| k != &key);
            changed = true;
        }
        changed
    }

    fn sync_mock_gif_frames(&mut self, page: &bbs_core::GifSearchPage, app: &mut App) {
        for item in &page.items {
            if let Some(ref frames) = item.preview_rgba_frames
                && !frames.is_empty()
            {
                app.images.insert(item.preview_url.clone(), frames[0].clone());
                let decoded = DecodedMedia::from_rgba_frames(
                    item.preview_url.clone(),
                    Some(format!("{}.gif", item.id)),
                    frames[0].width,
                    frames[0].height,
                    frames.clone(),
                );
                let arc = Arc::new(decoded);
                let _ = self.pool.insert_cached(Arc::clone(&arc));
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
        for slot in slots {
            if self.pool.get_cached(&slot.url).is_some() || self.in_flight.contains(&slot.url) {
                continue;
            }
            self.in_flight.insert(slot.url.clone());

            if let Some(ref rest) = services.rest_client {
                let rest = Arc::clone(rest);
                let pool = Arc::clone(&self.pool);
                let tx = self.decoded_tx.clone();
                let url = slot.url.clone();
                tokio::spawn(async move {
                    if let Ok((bytes, _)) = rest.fetch_attachment_bytes(&url).await {
                        let name = url
                            .rsplit('/')
                            .next()
                            .unwrap_or("image.bin")
                            .split('?')
                            .next()
                            .unwrap_or("image.bin")
                            .to_string();
                        if let Ok((decoded, _evicted)) =
                            pool.decode_and_cache(url.clone(), bytes, Some(name)).await
                        {
                            let _ = tx.send((url, decoded));
                        }
                    }
                });
            } else {
                // Offline mock mode: synthesize a crisp deterministic preview frame so pixel tiers render offline.
                let frame = synthesize_mock_preview_frame(&slot.url, 32, 24);
                app.images.insert(slot.url.clone(), frame.clone());
                let decoded = DecodedMedia::from_rgba_frames(
                    slot.url.clone(),
                    Some("mock-preview.png".to_string()),
                    frame.width,
                    frame.height,
                    vec![frame],
                );
                let _ = self.pool.insert_cached(Arc::new(decoded));
                self.in_flight.remove(&slot.url);
            }
        }
    }

    fn tick_animations(&mut self, slots: &[ImageSlot], app: &mut App, now: Instant) -> bool {
        let mut any_changed = false;
        for slot in slots {
            if !slot.animated {
                continue;
            }
            if let Some(player) = self.players.get_mut(&slot.url) {
                player.set_focused(slot.focused);
                if player.tick(now, &self.pacer).is_some() {
                    if let Some(frame) = player.current_frame() {
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

        if slots != self.last_slots.as_slice() {
            self.scroll_proxy.on_scroll(now);
            self.last_slots = slots.to_vec();
        }
        let use_proxy = is_resizing
            || self
                .scroll_proxy
                .should_use_proxy(self.renderer.tier(), now);

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
            let key = (
                slot.url.clone(),
                frame_idx,
                slot.visible.width,
                slot.visible.height,
                use_proxy,
            );

            if !self.encoded.contains_key(&key) {
                let rendered = if use_proxy {
                    self.renderer.render_halfblock_proxy(
                        &media,
                        frame_idx,
                        slot.visible.width,
                        slot.visible.height,
                        MediaFitMode::Contain,
                    )
                } else {
                    self.renderer.render_frame(
                        &media,
                        frame_idx,
                        slot.visible.width,
                        slot.visible.height,
                        MediaFitMode::Contain,
                    )
                };
                if let Ok(item) = rendered {
                    let est = item.estimated_pty_bytes();
                    if self.governor.can_emit(est, now) {
                        self.governor.record_emitted(est, now);
                        self.encoded.insert(key.clone(), item);
                    }
                }
            }

            if let Some(rendered) = self.encoded.get(&key) {
                let widget = MediaWidget::new(rendered);
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
    let guard_cfg = TtyGuardConfig::from_probe(&probe_report);
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
    let mut should_quit = false;

    while !should_quit {
        let now = Instant::now();
        media.ingest_decoded(&mut app);

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
        let debounce_sleep = async {
            match debouncer.next_deadline() {
                Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
                None => std::future::pending::<()>().await,
            }
        };

        #[cfg(unix)]
        tokio::select! {
            biased;
            maybe_sig = signal_rx.recv() => {
                match maybe_sig {
                    Some(TerminalSignal::Resize(geom)) => {
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
                        let key = normalize_key_event(raw_key);
                        // Optional Shift-free clipboard / URL convenience chords in NORMAL mode:
                        if key.kind != KeyEventKind::Release
                            && key.modifiers.contains(KeyModifiers::ALT)
                            && key.code == KeyCode::Char('y')
                            && let Some(msg_text) = selected_message_text(&app)
                        {
                            let _ = copy_to_clipboard(&msg_text, osc52);
                        } else if key.kind != KeyEventKind::Release
                            && key.modifiers.contains(KeyModifiers::ALT)
                            && key.code == KeyCode::Char('o')
                            && let Some(url) = selected_message_url(&app)
                        {
                            let _ = open_external_url(&url);
                        } else {
                            effects.extend(app.handle_key(key));
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        effects.extend(app.handle_paste(&text));
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        let geom = query_stdout_winsize()
                            .unwrap_or_else(|| ViewportGeometry::new(cols, rows));
                        if let Some(px) = geom.cell_pixels() {
                            app.cfg.cell_px = px;
                            media.renderer.set_cell_pixels(px);
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
            () = debounce_sleep => {
                if let Some(settled) = debouncer.poll_settled_now() {
                    if let Some(px) = settled.geometry.cell_pixels() {
                        media.renderer.set_cell_pixels(px);
                    }
                    media.clear_encoded();
                }
            }
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
                        effects.extend(app.handle_key(normalize_key_event(raw_key)));
                    }
                    Some(Ok(Event::Paste(text))) => {
                        effects.extend(app.handle_paste(&text));
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
                    if let BackendEvent::VideoFrameReceived(ref frame) = ev {
                        media.ingest_video_frame(frame, &mut app);
                    }
                    effects.extend(app.handle_event(ev));
                }
            }
            () = debounce_sleep => {
                if debouncer.poll_settled_now().is_some() {
                    media.clear_encoded();
                }
            }
            _ = tick_interval.tick() => {
                tick_count = tick_count.saturating_add(1);
                effects.extend(app.tick(current_unix_ms()));
            }
        }

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
}
