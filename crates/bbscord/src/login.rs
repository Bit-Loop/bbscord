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
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt as _;
use qrcode::{Color as QrColor, QrCode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
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
    Qr,
    Token,
}

struct LoginUiState {
    tab: LoginTab,
    qr_url: Option<String>,
    qr_lines: Vec<String>,
    qr_status: String,
    token_buf: Zeroizing<String>,
    save_to_keyring: bool,
    error_msg: Option<String>,
}

/// Interactive terminal login dialog supporting Discord QR Remote-Auth v2 and masked token paste.
///
/// Must be called while the terminal is already inside [`bbs_runtime_term::TtyGuard`].
pub async fn run_interactive_login(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    fingerprint: DiscordFingerprint,
    charset: ResolvedCharset,
    keyring_store: &OsKeyringStore,
) -> Result<Option<ResolvedAuth>> {
    let (ra_tx, mut ra_rx) = mpsc::unbounded_channel::<RemoteAuthEvent>();
    let ra_client = RemoteAuthClient::new(fingerprint.clone());
    let mut ra_task = tokio::spawn(async move { ra_client.authenticate(ra_tx).await });

    let mut state = LoginUiState {
        tab: LoginTab::Qr,
        qr_url: None,
        qr_lines: Vec::new(),
        qr_status: "Connecting to Discord Remote-Auth Gateway v2...".to_string(),
        token_buf: Zeroizing::new(String::new()),
        save_to_keyring: true,
        error_msg: None,
    };

    let mut events = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(120));

    let result = loop {
        terminal.draw(|frame| draw_login_screen(frame, &state, charset))?;

        tokio::select! {
            maybe_ev = ra_rx.recv() => {
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
                            state.qr_status = "QR login cancelled on mobile. Press [R] to retry or [Tab] for token.".to_string();
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
            join_res = &mut ra_task, if !ra_task.is_finished() => {
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
                        state.qr_status = "Switch to [Tab] Token Paste or press [Esc] to quit.".to_string();
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
                        match k.code {
                            KeyCode::Esc => break Ok(None),
                            KeyCode::Tab | KeyCode::BackTab => {
                                state.tab = match state.tab {
                                    LoginTab::Qr => LoginTab::Token,
                                    LoginTab::Token => LoginTab::Qr,
                                };
                                state.error_msg = None;
                            }
                            KeyCode::Char('s' | 'S') if state.tab == LoginTab::Qr => {
                                state.save_to_keyring = !state.save_to_keyring;
                            }
                            KeyCode::Char('r' | 'R') if state.tab == LoginTab::Qr => {
                                ra_task.abort();
                                let (new_tx, new_rx) = mpsc::unbounded_channel::<RemoteAuthEvent>();
                                ra_rx = new_rx;
                                let retry_client = RemoteAuthClient::new(fingerprint.clone());
                                ra_task = tokio::spawn(async move {
                                    retry_client.authenticate(new_tx).await
                                });
                                state.qr_url = None;
                                state.qr_lines.clear();
                                state.error_msg = None;
                                state.qr_status =
                                    "Reconnecting to Discord Remote-Auth Gateway v2...".to_string();
                            }
                            KeyCode::Char('s') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                                state.save_to_keyring = !state.save_to_keyring;
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

    ra_task.abort();
    let _ = io::stdout().flush();
    result
}

fn draw_login_screen(frame: &mut ratatui::Frame<'_>, state: &LoginUiState, charset: ResolvedCharset) {
    let area = frame.area();
    if area.width < 12 || area.height < 4 {
        let msg = Paragraph::new("LOGIN: Tab=mode Esc=quit");
        frame.render_widget(msg, area);
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

    let qr_style = if state.tab == LoginTab::Qr {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Gray)
    };
    let tok_style = if state.tab == LoginTab::Token {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Gray)
    };

    let header = Paragraph::new(Line::from(vec![
        Span::styled(" bbscord AUTH GATEWAY  ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" [1] QR Remote-Auth ", qr_style),
        Span::raw("  "),
        Span::styled(" [2] Token Paste ", tok_style),
    ]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL).title(" LOGIN "));
    frame.render_widget(header, chunks[0]);

    let body_block = Block::default().borders(Borders::ALL).title(match state.tab {
        LoginTab::Qr => " QR REMOTE-AUTH v2 ",
        LoginTab::Token => " MANUAL TOKEN ENTRY (RAM ZEROIZED) ",
    });

    match state.tab {
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
            let masked: String = std::iter::repeat_n(mask_char, state.token_buf.chars().count()).collect();
            let mut lines = vec![
                Line::raw("Paste or type your Discord token (input is masked and zeroized in RAM):"),
                Line::raw(""),
                Line::styled(
                    format!("> {masked}_"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
        " {save_flag} Save to OS Keyring (bbscord/token) [S / Ctrl+S]  |  [Tab] Switch Method  |  [Esc] Quit"
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
}
