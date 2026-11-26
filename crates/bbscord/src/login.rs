//! Live-mode authentication resolver: OS keyring, opt-in RAM-only `discordo` import,
//! QR Remote-Auth v2 scannable terminal view, and masked token entry.
//!
//! Security invariants:
//! - NEVER reads live secrets during `--mock-demo` or `--headless-smoke`.
//! - `--import-discordo-keyring` reads `service="discordo", user="token"` strictly read-only
//!   into RAM and NEVER writes the imported token to disk or `bbscord`'s keyring.
//! - All secret tokens stay wrapped in [`SecretToken`] / [`Zeroizing`].

use std::io::{self, Write as _};
use std::time::Duration;

use anyhow::{Result, bail};
use bbs_core::{
    KeyringStore, OsKeyringStore, ResolvedCharset, SecretToken, TokenOrigin,
};
use bbs_protocol::{DiscordFingerprint, RemoteAuthClient, RemoteAuthEvent};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt as _;
use qrcode::{Color as QrColor, QrCode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// Resolved live authentication credential plus its non-secret provenance.
#[derive(Debug, Clone)]
pub struct ResolvedAuth {
    /// Zeroizing Discord user token.
    pub token: SecretToken,
    /// Where the token originated (safe to log).
    pub origin: TokenOrigin,
}

/// Resolve a Discord authentication token from the OS keyring or opt-in RAM-only `discordo` import.
///
/// Returns `Ok(None)` when no stored credential is available and interactive login should run.
pub fn try_resolve_stored_token(
    store: &dyn KeyringStore,
    import_discordo_keyring: bool,
) -> Result<Option<ResolvedAuth>> {
    match store.load_bbscord_token() {
        Ok(Some(token)) => {
            return Ok(Some(ResolvedAuth {
                token,
                origin: TokenOrigin::BbscordKeyring,
            }));
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(error = %err, "bbscord OS keyring lookup unavailable; continuing");
        }
    }

    if import_discordo_keyring {
        match store.import_discordo_token_ram_only() {
            Ok(Some(token)) => {
                // Strictly RAM-only: NEVER saved to disk or copied automatically.
                return Ok(Some(ResolvedAuth {
                    token,
                    origin: TokenOrigin::DiscordoReadOnlyRamImport,
                }));
            }
            Ok(None) => {
                tracing::warn!("--import-discordo-keyring requested, but no discordo keyring entry was found");
            }
            Err(err) => {
                tracing::warn!(error = %err, "discordo OS keyring read-only import unavailable");
            }
        }
    }

    Ok(None)
}

/// Render a URL into compact terminal QR lines using half-blocks (`▀`/`▄`/`█`/` `) for UTF-8
/// or `##`/`  ` pairs for strict 7-bit ASCII terminals.
#[must_use]
pub fn render_qr_lines(url: &str, charset: ResolvedCharset) -> Vec<String> {
    let Ok(code) = QrCode::new(url.as_bytes()) else {
        return vec![format!("[QR encode failed — open manually: {url}]")];
    };
    let width = code.width();
    let quiet = 1usize;
    let total = width + quiet * 2;
    let is_dark = |x: usize, y: usize| -> bool {
        if x < quiet || y < quiet || x >= quiet + width || y >= quiet + width {
            false
        } else {
            code[(x - quiet, y - quiet)] == QrColor::Dark
        }
    };

    if charset == ResolvedCharset::Ascii7Bit {
        let mut lines = Vec::with_capacity(total);
        for y in 0..total {
            let mut row = String::with_capacity(total * 2);
            for x in 0..total {
                row.push_str(if is_dark(x, y) { "##" } else { "  " });
            }
            lines.push(row);
        }
        return lines;
    }

    // Half-block packing: 2 vertical QR modules per terminal row (quiet zone = light border).
    let mut lines = Vec::with_capacity(total.div_ceil(2));
    let mut y = 0usize;
    while y < total {
        let mut row = String::with_capacity(total);
        for x in 0..total {
            let top = is_dark(x, y);
            let bottom = if y + 1 < total {
                is_dark(x, y + 1)
            } else {
                false
            };
            // Light background (white) with dark modules (black) for optimal mobile scanner contrast.
            let ch = match (top, bottom) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            };
            row.push(ch);
        }
        lines.push(row);
        y += 2;
    }
    lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginTab {
    Import,
    Qr,
    Token,
}

impl LoginTab {
    const fn next(self) -> Self {
        match self {
            Self::Import => Self::Qr,
            Self::Qr => Self::Token,
            Self::Token => Self::Import,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Import => Self::Token,
            Self::Qr => Self::Import,
            Self::Token => Self::Qr,
        }
    }
}

/// Source descriptor for an auto-detected local Discord session candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedSessionKind {
    /// Existing `discordo` entry in the OS keyring (`service="discordo", user="token"`).
    DiscordoKeyring,
    /// Process environment variable (`BBSCORD_TOKEN`).
    EnvBbscordToken,
    /// Process environment variable (`DISCORD_TOKEN`).
    EnvDiscordToken,
}

impl DetectedSessionKind {
    /// Human-readable label shown in the initial setup login picker.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DiscordoKeyring => "Discordo (OS Keyring — discordo/token)",
            Self::EnvBbscordToken => "Environment ($BBSCORD_TOKEN)",
            Self::EnvDiscordToken => "Environment ($DISCORD_TOKEN)",
        }
    }

    /// Compact label for `16×2` micro displays.
    #[must_use]
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::DiscordoKeyring => "discordo",
            Self::EnvBbscordToken => "$BBSCORD",
            Self::EnvDiscordToken => "$DISCORD",
        }
    }

    /// Non-secret provenance tag recorded on [`ResolvedAuth`].
    #[must_use]
    pub const fn origin(self) -> TokenOrigin {
        match self {
            Self::DiscordoKeyring => TokenOrigin::DiscordoReadOnlyRamImport,
            Self::EnvBbscordToken | Self::EnvDiscordToken => TokenOrigin::InteractivePromptRamOnly,
        }
    }
}

/// An auto-detected local session candidate stored strictly in zeroized RAM.
#[derive(Debug, Clone)]
pub struct DetectedSession {
    /// Where this session was detected.
    pub kind: DetectedSessionKind,
    /// Zeroizing token loaded into RAM (redacted in `Debug`, never written unless saved).
    token: SecretToken,
}

impl DetectedSession {
    /// Construct a detected session candidate.
    #[must_use]
    pub const fn new(kind: DetectedSessionKind, token: SecretToken) -> Self {
        Self { kind, token }
    }
}

/// Probe sanctioned local session sources (OS keyring companion entries and explicit env vars)
/// strictly into RAM without mutating any keyring or writing to disk.
#[must_use]
pub fn detect_importable_sessions(
    store: &dyn KeyringStore,
    include_process_env: bool,
) -> Vec<DetectedSession> {
    let mut found = Vec::new();

    match store.import_discordo_token_ram_only() {
        Ok(Some(token)) => found.push(DetectedSession::new(
            DetectedSessionKind::DiscordoKeyring,
            token,
        )),
        Ok(None) => {}
        Err(err) => {
            tracing::debug!(error = %err, "discordo OS keyring probe returned no session");
        }
    }

    if include_process_env {
        for (var, kind) in [
            ("BBSCORD_TOKEN", DetectedSessionKind::EnvBbscordToken),
            ("DISCORD_TOKEN", DetectedSessionKind::EnvDiscordToken),
        ] {
            if let Ok(raw) = std::env::var(var)
                && let Ok(token) = SecretToken::new(raw)
            {
                found.push(DetectedSession::new(kind, token));
            }
        }
    }

    found
}

/// Commit a user-selected detected session, optionally saving a copy into `bbscord`'s own OS
/// keyring slot (`bbscord/token`) when `save_to_keyring` is enabled.
#[must_use]
pub fn finalize_imported_session(
    session: &DetectedSession,
    store: &dyn KeyringStore,
    save_to_keyring: bool,
) -> ResolvedAuth {
    let mut origin = session.kind.origin();
    if save_to_keyring {
        match store.save_bbscord_token(&session.token) {
            Ok(()) => {
                origin = TokenOrigin::BbscordKeyring;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "could not persist imported token to OS keyring; continuing in RAM"
                );
            }
        }
    }
    ResolvedAuth {
        token: session.token.clone(),
        origin,
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct LoginHitAreas {
    header: Rect,
    body: Rect,
    footer: Rect,
    import_list_start_y: u16,
}

struct LoginUiState {
    tab: LoginTab,
    detected: Vec<DetectedSession>,
    selected_import: usize,
    qr_url: Option<String>,
    qr_lines: Vec<String>,
    qr_status: String,
    token_buf: Zeroizing<String>,
    save_to_keyring: bool,
    error_msg: Option<String>,
    hits: LoginHitAreas,
}

/// Interactive terminal login dialog supporting auto-detected session import,
/// Discord QR Remote-Auth v2, and masked token paste.
///
/// Must be called while the terminal is already inside [`bbs_runtime_term::TtyGuard`].
pub async fn run_interactive_login(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    fingerprint: DiscordFingerprint,
    charset: ResolvedCharset,
    keyring_store: &OsKeyringStore,
) -> Result<Option<ResolvedAuth>> {
    let detected = detect_importable_sessions(keyring_store, true);
    let initial_tab = if detected.is_empty() {
        LoginTab::Qr
    } else {
        LoginTab::Import
    };

    let (mut ra_tx_opt, mut ra_rx) = mpsc::unbounded_channel::<RemoteAuthEvent>();
    let mut ra_task: Option<tokio::task::JoinHandle<bbs_core::CoreResult<SecretToken>>> = None;

    let spawn_qr = |fp: &DiscordFingerprint,
                    rx_slot: &mut mpsc::UnboundedReceiver<RemoteAuthEvent>,
                    task_slot: &mut Option<
                        tokio::task::JoinHandle<bbs_core::CoreResult<SecretToken>>,
                    >| {
        if let Some(old) = task_slot.take() {
            old.abort();
        }
        let (tx, rx) = mpsc::unbounded_channel::<RemoteAuthEvent>();
        *rx_slot = rx;
        let client = RemoteAuthClient::new(fp.clone());
        *task_slot = Some(tokio::spawn(async move { client.authenticate(tx).await }));
    };

    if initial_tab == LoginTab::Qr {
        let client = RemoteAuthClient::new(fingerprint.clone());
        let tx = ra_tx_opt;
        ra_task = Some(tokio::spawn(async move { client.authenticate(tx).await }));
        // Replace placeholder sender to avoid unused warning.
        let (fresh_tx, _) = mpsc::unbounded_channel::<RemoteAuthEvent>();
        ra_tx_opt = fresh_tx;
    }
    drop(ra_tx_opt);

    let mut state = LoginUiState {
        tab: initial_tab,
        detected,
        selected_import: 0,
        qr_url: None,
        qr_lines: Vec::new(),
        qr_status: "Connecting to Discord Remote-Auth Gateway v2...".to_string(),
        token_buf: Zeroizing::new(String::new()),
        save_to_keyring: true,
        error_msg: None,
        hits: LoginHitAreas::default(),
    };

    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(120));

    let result = loop {
        terminal.draw(|frame| draw_login_screen(frame, &mut state, charset))?;

        let has_active_qr = ra_task.as_ref().is_some_and(|h| !h.is_finished());

        tokio::select! {
            maybe_ev = ra_rx.recv(), if ra_task.is_some() => {
                if let Some(ev) = maybe_ev {
                    match ev {
                        RemoteAuthEvent::QrReady { qr_url, .. } => {
                            state.qr_lines = render_qr_lines(&qr_url, charset);
                            state.qr_url = Some(qr_url);
                            state.qr_status = "Scan QR with Discord mobile app to authorize login.".to_string();
                            state.error_msg = None;
                        }
                        RemoteAuthEvent::PendingConfirmation(user) => {
                            state.qr_status = format!(
                                "Scanned by {} — confirm login on your phone...",
                                user.display_handle()
                            );
                        }
                        RemoteAuthEvent::Cancelled => {
                            state.qr_status = "QR login cancelled on mobile. Press [R] to retry or [Tab] to switch.".to_string();
                        }
                        RemoteAuthEvent::Authenticated(token) => {
                            if state.save_to_keyring
                                && let Err(err) = keyring_store.save_bbscord_token(&token)
                            {
                                tracing::warn!(error = %err, "could not persist token to OS keyring; continuing in RAM");
                            }
                            break Ok(Some(ResolvedAuth {
                                token,
                                origin: TokenOrigin::InteractivePromptRamOnly,
                            }));
                        }
                    }
                }
            }
            join_res = async {
                if let Some(handle) = ra_task.as_mut() {
                    handle.await
                } else {
                    std::future::pending().await
                }
            }, if has_active_qr => {
                ra_task = None;
                match join_res {
                    Ok(Ok(token)) => {
                        if state.save_to_keyring
                            && let Err(err) = keyring_store.save_bbscord_token(&token)
                        {
                            tracing::warn!(error = %err, "could not persist token to OS keyring; continuing in RAM");
                        }
                        break Ok(Some(ResolvedAuth {
                            token,
                            origin: TokenOrigin::InteractivePromptRamOnly,
                        }));
                    }
                    Ok(Err(err)) => {
                        state.error_msg = Some(format!("QR auth error: {err}"));
                        state.qr_status = "Press [R] to retry, or [Tab] for Import / Token Paste.".to_string();
                    }
                    Err(err) => {
                        state.error_msg = Some(format!("QR auth task aborted: {err}"));
                    }
                }
            }
            maybe_term = events.next() => {
                let Some(Ok(ev)) = maybe_term else {
                    break Ok(None);
                };
                match ev {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                            break Ok(None);
                        }
                        let prev_tab = state.tab;
                        match k.code {
                            KeyCode::Esc => break Ok(None),
                            KeyCode::Tab => {
                                state.tab = state.tab.next();
                                state.error_msg = None;
                            }
                            KeyCode::BackTab => {
                                state.tab = state.tab.prev();
                                state.error_msg = None;
                            }
                            KeyCode::Left if state.tab != LoginTab::Token => {
                                state.tab = state.tab.prev();
                                state.error_msg = None;
                            }
                            KeyCode::Right if state.tab != LoginTab::Token => {
                                state.tab = state.tab.next();
                                state.error_msg = None;
                            }
                            KeyCode::Char('1') if state.tab != LoginTab::Token => {
                                state.tab = LoginTab::Import;
                                state.error_msg = None;
                            }
                            KeyCode::Char('2') if state.tab != LoginTab::Token => {
                                state.tab = LoginTab::Qr;
                                state.error_msg = None;
                            }
                            KeyCode::Char('3') if state.tab != LoginTab::Token => {
                                state.tab = LoginTab::Token;
                                state.error_msg = None;
                            }
                            KeyCode::Char('s' | 'S' | ' ') if state.tab != LoginTab::Token => {
                                state.save_to_keyring = !state.save_to_keyring;
                            }
                            KeyCode::Char('s') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                state.save_to_keyring = !state.save_to_keyring;
                            }
                            KeyCode::Up | KeyCode::Char('k') if state.tab == LoginTab::Import => {
                                state.selected_import = state.selected_import.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j')
                                if state.tab == LoginTab::Import
                                    && state.selected_import + 1 < state.detected.len() =>
                            {
                                state.selected_import += 1;
                            }
                            KeyCode::Char('r' | 'R') if state.tab == LoginTab::Import => {
                                state.detected = detect_importable_sessions(keyring_store, true);
                                state.selected_import = 0;
                                state.error_msg = None;
                            }
                            KeyCode::Enter if state.tab == LoginTab::Import => {
                                if let Some(chosen) = state.detected.get(state.selected_import) {
                                    let resolved = finalize_imported_session(
                                        chosen,
                                        keyring_store,
                                        state.save_to_keyring,
                                    );
                                    break Ok(Some(resolved));
                                }
                                state.error_msg = Some(
                                    "No local session detected — press [2] for QR or [3] for Token Paste."
                                        .to_string(),
                                );
                            }
                            KeyCode::Char('r' | 'R') if state.tab == LoginTab::Qr => {
                                spawn_qr(&fingerprint, &mut ra_rx, &mut ra_task);
                                state.qr_url = None;
                                state.qr_lines.clear();
                                state.error_msg = None;
                                state.qr_status =
                                    "Reconnecting to Discord Remote-Auth Gateway v2...".to_string();
                            }
                            KeyCode::Backspace if state.tab == LoginTab::Token => {
                                state.token_buf.pop();
                            }
                            KeyCode::Enter if state.tab == LoginTab::Token => {
                                match SecretToken::new(state.token_buf.as_str()) {
                                    Ok(token) => {
                                        state.token_buf.clear();
                                        if state.save_to_keyring
                                            && let Err(err) = keyring_store.save_bbscord_token(&token)
                                        {
                                            tracing::warn!(error = %err, "could not persist token to OS keyring; continuing in RAM");
                                        }
                                        break Ok(Some(ResolvedAuth {
                                            token,
                                            origin: TokenOrigin::InteractivePromptRamOnly,
                                        }));
                                    }
                                    Err(_) => {
                                        state.error_msg = Some("Token cannot be empty.".to_string());
                                    }
                                }
                            }
                            KeyCode::Char(ch)
                                if state.tab == LoginTab::Token
                                    && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                    && !ch.is_control() =>
                            {
                                state.token_buf.push(ch);
                            }
                            _ => {}
                        }

                        // Lazily start QR Remote-Auth Gateway only once the user opens the QR tab.
                        if state.tab == LoginTab::Qr && prev_tab != LoginTab::Qr && ra_task.is_none() && state.qr_url.is_none() {
                            spawn_qr(&fingerprint, &mut ra_rx, &mut ra_task);
                            state.qr_status =
                                "Connecting to Discord Remote-Auth Gateway v2...".to_string();
                        }
                    }
                    Event::Mouse(m) if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) => {
                        let (cx, cy) = (m.column, m.row);
                        let prev_tab = state.tab;
                        if rect_contains(state.hits.header, cx, cy) && state.hits.header.width >= 12 {
                            let rel = cx.saturating_sub(state.hits.header.x);
                            let third = state.hits.header.width / 3;
                            state.tab = if rel < third {
                                LoginTab::Import
                            } else if rel < third.saturating_mul(2) {
                                LoginTab::Qr
                            } else {
                                LoginTab::Token
                            };
                            state.error_msg = None;
                        } else if rect_contains(state.hits.footer, cx, cy) {
                            state.save_to_keyring = !state.save_to_keyring;
                        } else if state.tab == LoginTab::Import
                            && rect_contains(state.hits.body, cx, cy)
                            && cy >= state.hits.import_list_start_y
                        {
                            let idx = usize::from(cy - state.hits.import_list_start_y);
                            if let Some(chosen) = state.detected.get(idx) {
                                state.selected_import = idx;
                                let resolved = finalize_imported_session(
                                    chosen,
                                    keyring_store,
                                    state.save_to_keyring,
                                );
                                break Ok(Some(resolved));
                            }
                        }

                        if state.tab == LoginTab::Qr && prev_tab != LoginTab::Qr && ra_task.is_none() && state.qr_url.is_none() {
                            spawn_qr(&fingerprint, &mut ra_rx, &mut ra_task);
                            state.qr_status =
                                "Connecting to Discord Remote-Auth Gateway v2...".to_string();
                        }
                    }
                    Event::Paste(pasted) if state.tab == LoginTab::Token => {
                        let cleaned: String = pasted.chars().filter(|c| !c.is_control()).collect();
                        state.token_buf.push_str(&cleaned);
                    }
                    _ => {}
                }
            }
            _ = redraw.tick() => {}
        }
    };

    if let Some(handle) = ra_task {
        handle.abort();
    }
    let _ = io::stdout().flush();
    result
}

const fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

fn draw_login_screen(
    frame: &mut ratatui::Frame<'_>,
    state: &mut LoginUiState,
    charset: ResolvedCharset,
) {
    let area = frame.area();
    if area.width < 24 || area.height < 6 {
        let micro_text = match state.tab {
            LoginTab::Import => state
                .detected
                .get(state.selected_import)
                .map_or_else(
                    || "IMPORT:none\nTab=QR/Tok Esc=q".to_string(),
                    |s| format!("IMPORT:{}\nEnt=go Tab=mode", s.kind.short_label()),
                ),
            LoginTab::Qr => "QR:need wider\nTab=mode Esc=q".to_string(),
            LoginTab::Token => {
                let len = state.token_buf.chars().count();
                format!("TOK:{len}ch\nEnt=go Tab=mode")
            }
        };
        frame.render_widget(Paragraph::new(micro_text), area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);

    state.hits.header = chunks[0];
    state.hits.body = chunks[1];
    state.hits.footer = chunks[2];

    let tab_style = |active: bool| {
        if active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Gray)
        }
    };

    let import_label = if state.detected.is_empty() {
        " [1] Import Session ".to_string()
    } else {
        format!(" [1] Import ({} found) ", state.detected.len())
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " bbscord AUTH  ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(import_label, tab_style(state.tab == LoginTab::Import)),
        Span::raw(" "),
        Span::styled(" [2] QR ", tab_style(state.tab == LoginTab::Qr)),
        Span::raw(" "),
        Span::styled(" [3] Token ", tab_style(state.tab == LoginTab::Token)),
    ]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL).title(" INITIAL SETUP "));
    frame.render_widget(header, chunks[0]);

    let body_block = Block::default().borders(Borders::ALL).title(match state.tab {
        LoginTab::Import => " IMPORT EXISTING LOCAL SESSION ",
        LoginTab::Qr => " QR REMOTE-AUTH v2 ",
        LoginTab::Token => " MANUAL TOKEN ENTRY (RAM ZEROIZED) ",
    });

    match state.tab {
        LoginTab::Import => {
            let dot = if charset == ResolvedCharset::Ascii7Bit {
                "*"
            } else {
                "●"
            };
            let arrow = if charset == ResolvedCharset::Ascii7Bit {
                ">"
            } else {
                "▶"
            };
            let mut lines: Vec<Line<'_>> = vec![
                Line::raw("Auto-detected local sessions (read into zeroized RAM; click or press [Enter] to sign in):"),
                Line::raw(""),
            ];
            // Top border (+1) + 2 intro lines = row y where session index 0 renders.
            state.hits.import_list_start_y = chunks[1].y.saturating_add(3);

            if state.detected.is_empty() {
                lines.push(Line::styled(
                    "No Discordo OS keyring or $BBSCORD_TOKEN/$DISCORD_TOKEN session detected.",
                    Style::default().fg(Color::Yellow),
                ));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Tip: Press [2] for instant mobile QR login, [3] for Token Paste, or [R] to re-scan.",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                for (idx, sess) in state.detected.iter().enumerate() {
                    let selected = idx == state.selected_import;
                    let prefix = if selected { arrow } else { " " };
                    let row_style = if selected {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    lines.push(Line::styled(
                        format!(" {prefix} {dot} {}   [DETECTED] ", sess.kind.label()),
                        row_style,
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Press [Enter] or click a session to connect. Toggle [S] to save into bbscord's keyring.",
                    Style::default().fg(Color::DarkGray),
                ));
                lines.push(Line::styled(
                    "Press [R] to re-scan  |  [2] QR Login (any official Discord mobile session)",
                    Style::default().fg(Color::DarkGray),
                ));
            }

            if let Some(err) = &state.error_msg {
                lines.push(Line::raw(""));
                lines.push(Line::styled(err.clone(), Style::default().fg(Color::Red)));
            }
            let p = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(body_block);
            frame.render_widget(p, chunks[1]);
        }
        LoginTab::Qr => {
            let mut lines: Vec<Line<'_>> = Vec::new();
            lines.push(Line::styled(
                state.qr_status.clone(),
                Style::default().fg(Color::Green),
            ));
            if !state.qr_lines.is_empty() {
                for row in &state.qr_lines {
                    lines.push(Line::raw(row.clone()));
                }
            }
            if let Some(url) = &state.qr_url {
                lines.push(Line::styled(
                    format!("URL: {url}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(err) = &state.error_msg {
                lines.push(Line::styled(err.clone(), Style::default().fg(Color::Red)));
            }
            let p = Paragraph::new(lines)
                .alignment(Alignment::Center)
                .block(body_block);
            frame.render_widget(p, chunks[1]);
        }
        LoginTab::Token => {
            let mask_char = if charset == ResolvedCharset::Ascii7Bit {
                '*'
            } else {
                '•'
            };
            let masked: String =
                std::iter::repeat_n(mask_char, state.token_buf.chars().count()).collect();
            let mut lines = vec![
                Line::raw("Paste or type your Discord token (input is masked and zeroized in RAM):"),
                Line::raw(""),
                Line::styled(
                    format!("> {masked}_"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::raw(""),
                Line::styled(
                    "Press [Enter] to authenticate. Use --mock-demo for offline testing.",
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if let Some(err) = &state.error_msg {
                lines.push(Line::raw(""));
                lines.push(Line::styled(err.clone(), Style::default().fg(Color::Red)));
            }
            let p = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(body_block);
            frame.render_widget(p, chunks[1]);
        }
    }

    let save_flag = if state.save_to_keyring { "[X]" } else { "[ ]" };
    let footer = Paragraph::new(format!(
        " {save_flag} Save to OS Keyring (bbscord/token) [S/Ctrl+S] | [Tab/1-3] Mode | [Esc] Quit"
    ))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, chunks[2]);
}

/// Ensure stdout is attached to an interactive TTY before launching live login / TUI.
pub fn ensure_interactive_tty() -> Result<()> {
    use std::io::IsTerminal as _;
    if !io::stdout().is_terminal() {
        bail!(
            "stdout is not an interactive TTY; use `--headless-smoke` for non-interactive verification"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbs_core::{
        BBSCORD_KEYRING_SERVICE, BBSCORD_KEYRING_USER, DISCORDO_KEYRING_SERVICE,
        DISCORDO_KEYRING_USER, MemoryKeyringStore,
    };

    #[test]
    fn stored_token_resolution_respects_opt_in_discordo_import() {
        let store = MemoryKeyringStore::new();
        assert!(try_resolve_stored_token(&store, false).unwrap().is_none());

        // Seed only external discordo slot in the in-memory double.
        let ext = SecretToken::new("unit-test-ram-only-token").unwrap();
        store
            .set_secret(DISCORDO_KEYRING_SERVICE, DISCORDO_KEYRING_USER, &ext)
            .unwrap();

        // Without opt-in flag, MUST NOT read discordo keyring.
        assert!(try_resolve_stored_token(&store, false).unwrap().is_none());

        // With opt-in flag, reads into RAM only and NEVER writes to bbscord slot.
        let resolved = try_resolve_stored_token(&store, true).unwrap().unwrap();
        assert_eq!(resolved.origin, TokenOrigin::DiscordoReadOnlyRamImport);
        assert_eq!(resolved.token.expose_secret(), "unit-test-ram-only-token");
        assert!(store.load_bbscord_token().unwrap().is_none());

        // Own bbscord slot takes precedence when present.
        let own = SecretToken::new("unit-test-own-bbscord-token").unwrap();
        store
            .set_secret(BBSCORD_KEYRING_SERVICE, BBSCORD_KEYRING_USER, &own)
            .unwrap();
        let resolved_own = try_resolve_stored_token(&store, true).unwrap().unwrap();
        assert_eq!(resolved_own.origin, TokenOrigin::BbscordKeyring);
        assert_eq!(
            resolved_own.token.expose_secret(),
            "unit-test-own-bbscord-token"
        );
    }

    #[test]
    fn qr_renderer_supports_utf8_and_ascii7bit() {
        let utf8 = render_qr_lines("https://discord.com/ra/test-fp", ResolvedCharset::Utf8);
        assert!(utf8.len() >= 10);

        let ascii = render_qr_lines("https://discord.com/ra/test-fp", ResolvedCharset::Ascii7Bit);
        assert!(ascii.len() >= 20);
        for line in &ascii {
            assert!(line.bytes().all(|b| b == b'#' || b == b' '));
        }
    }

    #[test]
    fn detect_and_finalize_imported_session_respects_save_toggle() {
        let store = MemoryKeyringStore::new();
        assert!(detect_importable_sessions(&store, false).is_empty());

        let ext = SecretToken::new("unit-test-detected-discordo-token").unwrap();
        store
            .set_secret(DISCORDO_KEYRING_SERVICE, DISCORDO_KEYRING_USER, &ext)
            .unwrap();

        let detected = detect_importable_sessions(&store, false);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].kind, DetectedSessionKind::DiscordoKeyring);

        // RAM-only import (save_to_keyring = false) MUST NOT write bbscord/token.
        let ram_only = finalize_imported_session(&detected[0], &store, false);
        assert_eq!(ram_only.origin, TokenOrigin::DiscordoReadOnlyRamImport);
        assert!(store.load_bbscord_token().unwrap().is_none());

        // Saving import (save_to_keyring = true) persists into bbscord/token for future runs.
        let saved = finalize_imported_session(&detected[0], &store, true);
        assert_eq!(saved.origin, TokenOrigin::BbscordKeyring);
        assert_eq!(
            store
                .load_bbscord_token()
                .unwrap()
                .unwrap()
                .expose_secret(),
            "unit-test-detected-discordo-token"
        );
    }

    #[test]
    fn login_screen_renders_cleanly_at_16x2_and_80x24() {
        use ratatui::backend::TestBackend;

        let ext = SecretToken::new("unit-test-render-token").unwrap();
        let mut state = LoginUiState {
            tab: LoginTab::Import,
            detected: vec![DetectedSession::new(
                DetectedSessionKind::DiscordoKeyring,
                ext,
            )],
            selected_import: 0,
            qr_url: None,
            qr_lines: Vec::new(),
            qr_status: String::new(),
            token_buf: Zeroizing::new(String::new()),
            save_to_keyring: true,
            error_msg: None,
            hits: LoginHitAreas::default(),
        };

        for (w, h, cs) in [
            (16, 2, ResolvedCharset::Ascii7Bit),
            (80, 24, ResolvedCharset::Utf8),
        ] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| draw_login_screen(f, &mut state, cs)).unwrap();
        }
    }
}
