//! Bulletproof raw-mode, bracketed paste, Kitty keyboard, `SIGTSTP`/`SIGCONT`, and panic-safe
//! terminal restoration guard.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};

use bbs_core::viewport::{KeyboardProtocol, ResolvedCharset, ViewportGeometry};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::probe::{ProbeReport, query_stdout_winsize};

/// Alternate screen enter (`CSI ? 1049 h`).
pub const ENTER_ALT_SCREEN_SEQ: &[u8] = b"\x1b[?1049h";

/// Alternate screen leave (`CSI ? 1049 l`).
pub const LEAVE_ALT_SCREEN_SEQ: &[u8] = b"\x1b[?1049l";

/// Disable DEC auto-wrap (`DECAWM`, `CSI ? 7 l`) so writing the bottom-right cell of a `16×2`
/// display cannot trigger scrolling.
pub const DISABLE_LINE_WRAP_SEQ: &[u8] = b"\x1b[?7l";

/// Re-enable DEC auto-wrap (`DECAWM`, `CSI ? 7 h`). MUST be emitted before [`LEAVE_ALT_SCREEN_SEQ`].
pub const ENABLE_LINE_WRAP_SEQ: &[u8] = b"\x1b[?7h";

/// Hide cursor (`CSI ? 25 l`).
pub const HIDE_CURSOR_SEQ: &[u8] = b"\x1b[?25l";

/// Show cursor (`CSI ? 25 h`).
pub const SHOW_CURSOR_SEQ: &[u8] = b"\x1b[?25h";

/// Enable bracketed paste (`CSI ? 2004 h`).
pub const ENABLE_BRACKETED_PASTE_SEQ: &[u8] = b"\x1b[?2004h";

/// Disable bracketed paste (`CSI ? 2004 l`).
pub const DISABLE_BRACKETED_PASTE_SEQ: &[u8] = b"\x1b[?2004l";

/// Enable focus in/out events (`CSI ? 1004 h`).
pub const ENABLE_FOCUS_EVENTS_SEQ: &[u8] = b"\x1b[?1004h";

/// Disable focus in/out events (`CSI ? 1004 l`).
pub const DISABLE_FOCUS_EVENTS_SEQ: &[u8] = b"\x1b[?1004l";

/// Enable SGR mouse button + drag tracking (`?1000h ?1002h ?1006h`).
pub const ENABLE_MOUSE_CAPTURE_SEQ: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// Disable all common mouse tracking modes defensively.
pub const DISABLE_MOUSE_CAPTURE_SEQ: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// Enable DEC grapheme-cluster width mode (`CSI ? 2027 h`).
pub const ENABLE_GRAPHEME_MODE_2027_SEQ: &[u8] = b"\x1b[?2027h";

/// Disable DEC grapheme-cluster width mode (`CSI ? 2027 l`).
pub const DISABLE_GRAPHEME_MODE_2027_SEQ: &[u8] = b"\x1b[?2027l";

/// Begin DEC synchronized output frame (`CSI ? 2026 h`).
pub const BEGIN_SYNC_OUTPUT_SEQ: &[u8] = b"\x1b[?2026h";

/// End DEC synchronized output frame (`CSI ? 2026 l`).
pub const END_SYNC_OUTPUT_SEQ: &[u8] = b"\x1b[?2026l";

/// Push progressive Kitty keyboard enhancement flags (`DISAMBIGUATE_ESCAPE_CODES = 1`, `CSI > 1 u`).
///
/// Disambiguates `Ctrl+Enter`, `Shift+Enter`, and standalone `Esc` while keeping standard
/// ASCII and `0x7F`/`0x08` Backspace functional everywhere.
pub const PUSH_KITTY_KEYBOARD_FLAGS_SEQ: &[u8] = b"\x1b[>1u";

/// Pop Kitty keyboard enhancement flags (`CSI < u`).
pub const POP_KITTY_KEYBOARD_FLAGS_SEQ: &[u8] = b"\x1b[<u";

/// Delete visible Kitty graphics placements (`APC G a=d ST`).
pub const KITTY_DELETE_PLACEMENTS_SEQ: &[u8] = b"\x1b_Ga=d\x1b\\";

/// Delete all Kitty graphics placements AND free stored pixel buffers silently (`d=A,q=2`).
pub const KITTY_DELETE_ALL_DATA_SEQ: &[u8] = b"\x1b_Ga=d,d=A,q=2\x1b\\";

/// Format a Kitty graphics deletion command that frees both placement and pixel memory (`d=I`)
/// for a specific image ID `id`.
#[must_use]
pub fn kitty_delete_image_data_seq(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// Configuration for [`TtyGuard`] terminal mode initialization and restoration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtyGuardConfig {
    /// Enable bracketed paste (`\x1b[?2004h`).
    pub enable_bracketed_paste: bool,
    /// Enable focus gained/lost events (`\x1b[?1004h`).
    pub enable_focus_events: bool,
    /// Enable SGR mouse capture (`\x1b[?1000h\x1b[?1002h\x1b[?1006h`).
    pub enable_mouse: bool,
    /// Push progressive Kitty keyboard disambiguation flags (`\x1b[>1u`).
    pub enable_kitty_keyboard: bool,
    /// Disable line auto-wrap (`DECAWM`, `\x1b[?7l`) inside alternate screen.
    pub disable_line_wrap: bool,
    /// Enable grapheme-cluster width mode (`\x1b[?2027h`) when confirmed by `DECRQM`.
    pub enable_grapheme_mode_2027: bool,
    /// Emit Kitty graphics delete sequence on teardown/suspend.
    pub delete_kitty_images_on_exit: bool,
    /// Install a once-only panic hook calling [`restore_terminal`].
    pub install_panic_hook: bool,
}

impl Default for TtyGuardConfig {
    fn default() -> Self {
        Self {
            enable_bracketed_paste: true,
            enable_focus_events: true,
            enable_mouse: true,
            enable_kitty_keyboard: false,
            disable_line_wrap: true,
            enable_grapheme_mode_2027: false,
            delete_kitty_images_on_exit: true,
            install_panic_hook: true,
        }
    }
}

impl TtyGuardConfig {
    /// Derive a safe [`TtyGuardConfig`] from a completed [`ProbeReport`].
    #[must_use]
    pub fn from_probe(report: &ProbeReport) -> Self {
        let caps = &report.capabilities;
        let allow_kitty_delete =
            report.resolved_charset != ResolvedCharset::Ascii7Bit && !report.vt_safe_charset;
        Self {
            enable_bracketed_paste: caps.bracketed_paste,
            enable_focus_events: caps.bracketed_paste && !report.vt_safe_charset,
            enable_mouse: caps.mouse_sgr,
            enable_kitty_keyboard: matches!(caps.keyboard, KeyboardProtocol::KittyCsiU),
            disable_line_wrap: true,
            enable_grapheme_mode_2027: report.grapheme_mode_2027,
            delete_kitty_images_on_exit: allow_kitty_delete,
            install_panic_hook: true,
        }
    }
}

/// Build the exact escape-sequence buffer emitted when entering TUI mode.
#[must_use]
pub fn build_enter_sequence(config: &TtyGuardConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(ENTER_ALT_SCREEN_SEQ);
    if config.disable_line_wrap {
        out.extend_from_slice(DISABLE_LINE_WRAP_SEQ);
    }
    out.extend_from_slice(HIDE_CURSOR_SEQ);
    if config.enable_bracketed_paste {
        out.extend_from_slice(ENABLE_BRACKETED_PASTE_SEQ);
    }
    if config.enable_focus_events {
        out.extend_from_slice(ENABLE_FOCUS_EVENTS_SEQ);
    }
    if config.enable_mouse {
        out.extend_from_slice(ENABLE_MOUSE_CAPTURE_SEQ);
    }
    if config.enable_grapheme_mode_2027 {
        out.extend_from_slice(ENABLE_GRAPHEME_MODE_2027_SEQ);
    }
    if config.enable_kitty_keyboard {
        out.extend_from_slice(PUSH_KITTY_KEYBOARD_FLAGS_SEQ);
    }
    out
}

/// Build the exact escape-sequence buffer emitted when tearing down or restoring the terminal.
///
/// Guarantees reverse-order teardown: deletes Kitty images, pops keyboard flags, disables
/// paste/mouse/focus, re-enables `DECAWM` (`?7h`) **before** leaving alternate screen (`?1049l`),
/// and restores cursor visibility.
#[must_use]
pub fn build_restore_sequence(config: &TtyGuardConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    if config.delete_kitty_images_on_exit {
        out.extend_from_slice(KITTY_DELETE_PLACEMENTS_SEQ);
        out.extend_from_slice(KITTY_DELETE_ALL_DATA_SEQ);
    }
    if config.enable_kitty_keyboard {
        out.extend_from_slice(POP_KITTY_KEYBOARD_FLAGS_SEQ);
    }
    if config.enable_grapheme_mode_2027 {
        out.extend_from_slice(DISABLE_GRAPHEME_MODE_2027_SEQ);
    }
    // Always terminate any in-flight synchronized output block defensively.
    out.extend_from_slice(END_SYNC_OUTPUT_SEQ);
    if config.enable_mouse {
        out.extend_from_slice(DISABLE_MOUSE_CAPTURE_SEQ);
    }
    if config.enable_focus_events {
        out.extend_from_slice(DISABLE_FOCUS_EVENTS_SEQ);
    }
    if config.enable_bracketed_paste {
        out.extend_from_slice(DISABLE_BRACKETED_PASTE_SEQ);
    }
    if config.disable_line_wrap {
        out.extend_from_slice(ENABLE_LINE_WRAP_SEQ);
    }
    out.extend_from_slice(SHOW_CURSOR_SEQ);
    out.extend_from_slice(LEAVE_ALT_SCREEN_SEQ);
    out
}

/// Build the escape sequence used to re-assert terminal modes on `Ctrl+L` full repaint.
#[must_use]
pub fn build_mode_refresh_sequence(config: &TtyGuardConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(96);
    if config.disable_line_wrap {
        out.extend_from_slice(DISABLE_LINE_WRAP_SEQ);
    }
    out.extend_from_slice(HIDE_CURSOR_SEQ);
    if config.enable_bracketed_paste {
        out.extend_from_slice(ENABLE_BRACKETED_PASTE_SEQ);
    }
    if config.enable_focus_events {
        out.extend_from_slice(ENABLE_FOCUS_EVENTS_SEQ);
    }
    if config.enable_mouse {
        out.extend_from_slice(ENABLE_MOUSE_CAPTURE_SEQ);
    }
    if config.enable_grapheme_mode_2027 {
        out.extend_from_slice(ENABLE_GRAPHEME_MODE_2027_SEQ);
    }
    if config.enable_kitty_keyboard {
        out.extend_from_slice(PUSH_KITTY_KEYBOARD_FLAGS_SEQ);
    }
    out
}

#[derive(Clone)]
struct SavedTerminalState {
    config: TtyGuardConfig,
    #[cfg(unix)]
    saved_termios: Option<libc::termios>,
}

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK_ONCE: Once = Once::new();
static SAVED_STATE: Mutex<Option<SavedTerminalState>> = Mutex::new(None);

#[cfg(unix)]
fn capture_stdin_termios() -> Option<libc::termios> {
    let mut t = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `tcgetattr` initializes `t` on success for `STDIN_FILENO`.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, t.as_mut_ptr()) } == 0 {
        // SAFETY: `tcgetattr` returned 0.
        Some(unsafe { t.assume_init() })
    } else {
        None
    }
}

#[cfg(unix)]
fn restore_stdin_termios(saved: &libc::termios) {
    // SAFETY: Restores a `termios` previously captured from `STDIN_FILENO`.
    let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, saved) };
}

/// Install a process-wide panic hook (once) that invokes [`restore_terminal`] before printing the
/// panic backtrace so panics never leave the user's shell stuck in raw/alternate-screen mode.
pub fn install_panic_hook() {
    PANIC_HOOK_ONCE.call_once(|| {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = restore_terminal();
            prev_hook(panic_info);
        }));
    });
}

/// Idempotently restore the terminal to cooked main-screen mode.
///
/// Safe to invoke from normal shutdown, `Drop`, `SIGTSTP` suspend handlers, and
/// `std::panic::set_hook`. Repeated calls after restoration are clean no-ops.
pub fn restore_terminal() -> io::Result<()> {
    restore_terminal_with_writer(&mut io::stdout())
}

/// Internal/testable variant of [`restore_terminal`] writing teardown sequences to `writer`.
pub fn restore_terminal_with_writer<W: Write>(writer: &mut W) -> io::Result<()> {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return Ok(());
    }

    let state = {
        let guard = SAVED_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    };

    let config = state
        .as_ref()
        .map(|s| s.config.clone())
        .unwrap_or_default();

    let seq = build_restore_sequence(&config);
    let write_res = writer.write_all(&seq).and_then(|()| writer.flush());

    #[cfg(unix)]
    if let Some(ref saved) = state.and_then(|s| s.saved_termios) {
        restore_stdin_termios(saved);
    }
    let _ = crossterm::terminal::disable_raw_mode();

    write_res
}

/// Synthetic event produced when returning from `SIGTSTP` / `SIGCONT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeNotification {
    /// Freshly queried viewport geometry after resume.
    pub geometry: ViewportGeometry,
    /// Caller must clear and repaint the full terminal buffer.
    pub needs_full_repaint: bool,
    /// Caller must re-upload/re-place graphics images cleared during suspend.
    pub needs_media_retransmit: bool,
}

/// RAII guard owning raw mode, alternate screen, bracketed paste, focus/mouse, and `SIGCONT` recovery.
#[derive(Debug)]
pub struct TtyGuard {
    config: TtyGuardConfig,
    suspended: bool,
    last_geometry: ViewportGeometry,
}

impl TtyGuard {
    /// Enter raw mode and configure alternate screen, bracketed paste, focus, mouse, and optional
    /// Kitty keyboard flags.
    pub fn enter(config: TtyGuardConfig) -> io::Result<Self> {
        if config.install_panic_hook {
            install_panic_hook();
        }

        #[cfg(unix)]
        let saved_termios = capture_stdin_termios();

        crossterm::terminal::enable_raw_mode()?;

        let mut stdout = io::stdout();
        let seq = build_enter_sequence(&config);
        if let Err(err) = stdout.write_all(&seq).and_then(|()| stdout.flush()) {
            #[cfg(unix)]
            if let Some(ref orig) = saved_termios {
                restore_stdin_termios(orig);
            }
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(err);
        }

        {
            let mut guard = SAVED_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(SavedTerminalState {
                config: config.clone(),
                #[cfg(unix)]
                saved_termios,
            });
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);

        let last_geometry = query_stdout_winsize().unwrap_or_default();

        Ok(Self {
            config,
            suspended: false,
            last_geometry,
        })
    }

    /// Return the active [`TtyGuardConfig`].
    #[must_use]
    pub fn config(&self) -> &TtyGuardConfig {
        &self.config
    }

    /// Returns `true` while the guard is currently suspended (`SIGTSTP`).
    #[must_use]
    pub const fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// Re-emit terminal mode sequences (`DECAWM`, bracketed paste, mouse, focus, Kitty kbd) on `Ctrl+L`.
    pub fn refresh_modes(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        let seq = build_mode_refresh_sequence(&self.config);
        stdout.write_all(&seq)?;
        stdout.flush()
    }

    /// Cleanly suspend the process on `Ctrl+Z`: restores cooked main screen, raises `SIGTSTP`,
    /// and upon `SIGCONT` resume re-initializes all modes and returns a synthetic resize/repaint event.
    pub fn suspend(&mut self) -> io::Result<ResumeNotification> {
        restore_terminal()?;
        self.suspended = true;

        #[cfg(unix)]
        {
            // SAFETY: Raising `SIGTSTP` suspends this process using standard POSIX job control
            // after the terminal has already been restored to cooked mode.
            let rc = unsafe { libc::raise(libc::SIGTSTP) };
            if rc != 0 {
                // Attempt recovery if raising failed.
                return self.resume_after_sigcont();
            }
        }

        self.resume_after_sigcont()
    }

    /// Re-initialize raw mode + all configured terminal modes after receiving `SIGCONT` and emit
    /// a synthetic resize + full repaint notification.
    pub fn resume_after_sigcont(&mut self) -> io::Result<ResumeNotification> {
        #[cfg(unix)]
        let saved_termios = capture_stdin_termios();

        crossterm::terminal::enable_raw_mode()?;

        let mut stdout = io::stdout();
        let seq = build_enter_sequence(&self.config);
        stdout.write_all(&seq)?;
        stdout.flush()?;

        {
            let mut guard = SAVED_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(SavedTerminalState {
                config: self.config.clone(),
                #[cfg(unix)]
                saved_termios: saved_termios.or_else(|| guard.as_ref().and_then(|s| s.saved_termios)),
            });
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        self.suspended = false;

        let geometry = query_stdout_winsize().unwrap_or(self.last_geometry);
        self.last_geometry = geometry;

        Ok(ResumeNotification {
            geometry,
            needs_full_repaint: true,
            needs_media_retransmit: true,
        })
    }

    /// Explicitly restore the terminal and consume the guard.
    pub fn restore(self) -> io::Result<()> {
        restore_terminal()
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        let _ = restore_terminal();
    }
}

/// Asynchronous POSIX terminal signals forwarded to the TUI event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSignal {
    /// `SIGWINCH` received with refreshed [`ViewportGeometry`].
    Resize(ViewportGeometry),
    /// `SIGCONT` received after external/job-control resume with refreshed [`ViewportGeometry`].
    Continued(ViewportGeometry),
    /// `SIGTERM` or `SIGHUP` requesting clean shutdown and terminal restoration.
    Terminate,
}

/// Spawn a Tokio task bridging Unix `SIGWINCH`, `SIGCONT`, `SIGTERM`, and `SIGHUP` signals into
/// a channel without blocking async workers.
#[cfg(unix)]
pub fn spawn_signal_listener() -> io::Result<mpsc::UnboundedReceiver<TerminalSignal>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigwinch = signal(SignalKind::window_change())?;
    let mut sigcont = signal(SignalKind::from_raw(libc::SIGCONT))?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sighup = signal(SignalKind::hangup())?;

    let (tx, rx) = mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(()) = sigwinch.recv() => {
                    let geom = query_stdout_winsize().unwrap_or_default();
                    if tx.send(TerminalSignal::Resize(geom)).is_err() {
                        break;
                    }
                }
                Some(()) = sigcont.recv() => {
                    let geom = query_stdout_winsize().unwrap_or_default();
                    if tx.send(TerminalSignal::Continued(geom)).is_err() {
                        break;
                    }
                }
                Some(()) = sigterm.recv() => {
                    let _ = tx.send(TerminalSignal::Terminate);
                    break;
                }
                Some(()) = sighup.recv() => {
                    let _ = tx.send(TerminalSignal::Terminate);
                    break;
                }
                else => break,
            }
        }
    });

    Ok(rx)
}

/// Normalize a `crossterm` [`KeyEvent`] so raw `0x7F` (`DEL`) and `0x08` (`BS`) are consistently
/// mapped to [`KeyCode::Backspace`] while preserving `Ctrl+J` (`LF`), `Ctrl+Enter`, and ASCII input.
#[must_use]
pub fn normalize_key_event(mut event: KeyEvent) -> KeyEvent {
    match event.code {
        KeyCode::Char('\x7f') | KeyCode::Char('\x08') => {
            event.code = KeyCode::Backspace;
            event.modifiers.remove(KeyModifiers::CONTROL);
        }
        KeyCode::Char('h' | 'H') if event.modifiers == KeyModifiers::CONTROL => {
            event.code = KeyCode::Backspace;
            event.modifiers = KeyModifiers::NONE;
        }
        _ => {}
    }
    event
}

/// Returns `true` if `event` represents a Backspace key (`Backspace`, raw `0x7F`, raw `0x08`, or `Ctrl+H`).
#[must_use]
pub fn is_backspace_key(event: KeyEvent) -> bool {
    matches!(normalize_key_event(event).code, KeyCode::Backspace)
}

/// Fallback paste heuristic for legacy terminals that ignore bracketed paste (`CSI ? 2004 h`).
///
/// A single burst read containing at least 2 line breaks (`\r` / `\n`) mixed with printable text
/// (and not beginning with an escape sequence) is classified as pasted multi-line content so it
/// inserts newlines instead of repeatedly firing `Enter = Send`.
#[must_use]
pub fn looks_like_unbracketed_paste(raw_chunk: &[u8]) -> bool {
    if raw_chunk.len() < 4 || raw_chunk.first() == Some(&0x1B) {
        return false;
    }
    let mut line_breaks = 0usize;
    let mut printable = 0usize;
    let mut prev_cr = false;

    for &b in raw_chunk {
        if b == b'\r' {
            line_breaks += 1;
            prev_cr = true;
        } else if b == b'\n' {
            if !prev_cr {
                line_breaks += 1;
            }
            prev_cr = false;
        } else {
            prev_cr = false;
            if (0x20..=0x7E).contains(&b) || b >= 0x80 {
                printable += 1;
            }
        }
    }

    line_breaks >= 2 && printable >= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    #[test]
    fn enter_and_restore_sequences_preserve_strict_ordering() {
        let cfg = TtyGuardConfig {
            enable_bracketed_paste: true,
            enable_focus_events: true,
            enable_mouse: true,
            enable_kitty_keyboard: true,
            disable_line_wrap: true,
            enable_grapheme_mode_2027: true,
            delete_kitty_images_on_exit: true,
            install_panic_hook: false,
        };

        let enter = build_enter_sequence(&cfg);
        let alt_in = find_subslice(&enter, ENTER_ALT_SCREEN_SEQ);
        let wrap_off = find_subslice(&enter, DISABLE_LINE_WRAP_SEQ);
        let paste_on = find_subslice(&enter, ENABLE_BRACKETED_PASTE_SEQ);
        let focus_on = find_subslice(&enter, ENABLE_FOCUS_EVENTS_SEQ);
        let mouse_on = find_subslice(&enter, ENABLE_MOUSE_CAPTURE_SEQ);
        let kbd_push = find_subslice(&enter, PUSH_KITTY_KEYBOARD_FLAGS_SEQ);

        assert!(alt_in.is_some());
        assert!(wrap_off.is_some());
        assert!(paste_on.is_some());
        assert!(focus_on.is_some());
        assert!(mouse_on.is_some());
        assert!(kbd_push.is_some());
        // DECAWM off MUST follow EnterAlternateScreen.
        assert!(alt_in < wrap_off);

        let restore = build_restore_sequence(&cfg);
        let kitty_del = find_subslice(&restore, KITTY_DELETE_PLACEMENTS_SEQ);
        let kitty_del_all = find_subslice(&restore, KITTY_DELETE_ALL_DATA_SEQ);
        let kbd_pop = find_subslice(&restore, POP_KITTY_KEYBOARD_FLAGS_SEQ);
        let paste_off = find_subslice(&restore, DISABLE_BRACKETED_PASTE_SEQ);
        let focus_off = find_subslice(&restore, DISABLE_FOCUS_EVENTS_SEQ);
        let mouse_off = find_subslice(&restore, DISABLE_MOUSE_CAPTURE_SEQ);
        let wrap_on = find_subslice(&restore, ENABLE_LINE_WRAP_SEQ);
        let cursor_show = find_subslice(&restore, SHOW_CURSOR_SEQ);
        let alt_out = find_subslice(&restore, LEAVE_ALT_SCREEN_SEQ);

        assert!(kitty_del.is_some());
        assert!(kitty_del_all.is_some());
        assert!(kbd_pop.is_some());
        assert!(paste_off.is_some());
        assert!(focus_off.is_some());
        assert!(mouse_off.is_some());
        assert!(wrap_on.is_some());
        assert!(cursor_show.is_some());
        assert!(alt_out.is_some());

        // Strict teardown ordering invariants:
        // Kitty delete & keyboard pop happen before leaving alt-screen;
        // DECAWM restore (?7h) MUST precede LeaveAlternateScreen (?1049l).
        assert!(kitty_del < alt_out);
        assert!(kbd_pop < alt_out);
        assert!(wrap_on < alt_out);
        assert!(cursor_show < alt_out);
    }

    #[test]
    fn restore_terminal_is_idempotent() {
        // Seed active state explicitly for deterministic test without touching real TTY.
        {
            let mut guard = SAVED_STATE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(SavedTerminalState {
                config: TtyGuardConfig::default(),
                #[cfg(unix)]
                saved_termios: None,
            });
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);

        let mut first = Vec::new();
        assert!(restore_terminal_with_writer(&mut first).is_ok());
        assert!(!first.is_empty());
        assert!(find_subslice(&first, KITTY_DELETE_PLACEMENTS_SEQ).is_some());
        assert!(find_subslice(&first, LEAVE_ALT_SCREEN_SEQ).is_some());

        // Second call MUST be a complete no-op.
        let mut second = Vec::new();
        assert!(restore_terminal_with_writer(&mut second).is_ok());
        assert!(second.is_empty());
    }

    #[test]
    fn backspace_normalization_handles_del_bs_and_ctrl_h_without_breaking_ctrl_j() {
        let make = |code, modifiers| KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert!(is_backspace_key(make(KeyCode::Backspace, KeyModifiers::NONE)));
        assert!(is_backspace_key(make(KeyCode::Char('\x7f'), KeyModifiers::NONE)));
        assert!(is_backspace_key(make(KeyCode::Char('\x08'), KeyModifiers::NONE)));
        assert!(is_backspace_key(make(KeyCode::Char('h'), KeyModifiers::CONTROL)));

        // Ctrl+J (LF newline) and Ctrl+Enter must remain intact!
        let ctrl_j = normalize_key_event(make(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(ctrl_j.code, KeyCode::Char('j'));
        assert_eq!(ctrl_j.modifiers, KeyModifiers::CONTROL);

        let ctrl_enter = normalize_key_event(make(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(ctrl_enter.code, KeyCode::Enter);
        assert_eq!(ctrl_enter.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn unbracketed_paste_heuristic_detects_multiline_bursts_only() {
        assert!(looks_like_unbracketed_paste(b"line one\nline two\nline three"));
        assert!(looks_like_unbracketed_paste(b"foo\r\nbar\r\nbaz"));
        assert!(!looks_like_unbracketed_paste(b"a"));
        assert!(!looks_like_unbracketed_paste(b"\r"));
        assert!(!looks_like_unbracketed_paste(b"single line\n"));
        assert!(!looks_like_unbracketed_paste(b"\x1b[A\n\n"));
    }

    #[test]
    fn kitty_delete_image_uses_uppercase_i_to_free_memory() {
        assert_eq!(kitty_delete_image_data_seq(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    }

    #[test]
    fn config_from_probe_and_mode_refresh_adapt_to_kitty_vs_ascii() {
        use crate::probe::{
            GeometrySource, ProbeOptions, ProbeResponse, TerminalEnv, negotiate_capabilities,
        };
        use bbs_core::viewport::CharsetMode;

        let mut env = TerminalEnv {
            term: Some("xterm-kitty".to_owned()),
            lang: Some("en_US.UTF-8".to_owned()),
            ..TerminalEnv::default()
        };
        let probe = ProbeResponse {
            kitty_graphics: true,
            da1_received: true,
            kitty_keyboard_flags: Some(1),
            grapheme_mode_2027: true,
            ..ProbeResponse::default()
        };
        let report = negotiate_capabilities(
            &env,
            ViewportGeometry::new(120, 40),
            GeometrySource::IoctlWinsize,
            &probe,
            &ProbeOptions::default(),
            false,
            false,
        );
        let cfg = TtyGuardConfig::from_probe(&report);
        assert!(cfg.enable_kitty_keyboard);
        assert!(cfg.enable_grapheme_mode_2027);
        assert!(cfg.enable_bracketed_paste);
        assert!(cfg.delete_kitty_images_on_exit);

        let refresh = build_mode_refresh_sequence(&cfg);
        assert!(find_subslice(&refresh, DISABLE_LINE_WRAP_SEQ).is_some());
        assert!(find_subslice(&refresh, ENABLE_BRACKETED_PASTE_SEQ).is_some());
        assert!(find_subslice(&refresh, PUSH_KITTY_KEYBOARD_FLAGS_SEQ).is_some());

        // 16x2 ASCII mode disables mouse, Kitty keyboard push, and Kitty APC deletion.
        env.term = Some("vt100".to_owned());
        let ascii_report = negotiate_capabilities(
            &env,
            ViewportGeometry::new(16, 2),
            GeometrySource::CliOverride,
            &ProbeResponse::default(),
            &ProbeOptions {
                charset_override: CharsetMode::Ascii7Bit,
                ..ProbeOptions::default()
            },
            false,
            true,
        );
        let ascii_cfg = TtyGuardConfig::from_probe(&ascii_report);
        assert!(!ascii_cfg.enable_mouse);
        assert!(!ascii_cfg.enable_kitty_keyboard);
        assert!(!ascii_cfg.delete_kitty_images_on_exit);
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }
}
