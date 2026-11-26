//! Non-blocking terminal capability, geometry, and graphics-tier negotiator.
//!
//! All active terminal probes run under a strict hard deadline (default `150 ms`) so dumb
//! UART links, hardware `16×2` HD44780 LCD bridges, and unresponsive PTYs never hang.

use std::env;
#[cfg(unix)]
use std::io;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use bbs_core::theme::ColorDepth;
use bbs_core::viewport::{
    CharsetMode, GraphicsTier, KeyboardProtocol, LayoutProfile, MultiplexerKind, ResolvedCharset,
    TerminalCapabilities, ViewportGeometry,
};

use crate::{TermError, TermResult};

/// Default hard deadline for active terminal capability/DSR probes (`150 ms`).
pub const DEFAULT_PROBE_DEADLINE: Duration = Duration::from_millis(150);

/// Local ESC-sequence disambiguation timeout when Kitty keyboard protocol is inactive.
pub const ESC_TIMEOUT_LOCAL_MS: u16 = 50;

/// SSH ESC-sequence disambiguation timeout (`$SSH_CONNECTION` / `$SSH_TTY`).
pub const ESC_TIMEOUT_SSH_MS: u16 = 100;

/// ESC-sequence disambiguation timeout when Kitty keyboard protocol (`CSI u`) is active.
pub const ESC_TIMEOUT_KITTY_KBD_MS: u16 = 0;

/// Fallback character cell dimensions in pixels when pixel metrics are unavailable (`1:2` aspect).
pub const DEFAULT_CELL_PIXELS: (u16, u16) = (8, 16);

/// Fallback cell pixel aspect ratio (`cell_width / cell_height = 0.5`).
pub const DEFAULT_CELL_ASPECT_RATIO: f32 = 0.5;

/// VT100 save cursor + clamp to `(999, 999)` + Device Status Report (`DSR`) query.
pub const VT100_CURSOR_CLAMP_QUERY: &[u8] = b"\x1b7\x1b[999;999H\x1b[6n";

/// VT100 restore cursor sequence (`DECRC`).
pub const VT100_CURSOR_RESTORE: &[u8] = b"\x1b8";

/// 1×1 Kitty graphics protocol capability query (`a=q`, `i=31`).
pub const KITTY_GRAPHICS_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";

/// 1×1 Kitty graphics zlib-compression capability query (`a=q`, `i=32`, `o=z`, zlib(`[0,0,0]`)).
pub const KITTY_ZLIB_QUERY: &[u8] = b"\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24,o=z;eJxjYGAAAAADAAE=\x1b\\";

/// DCS-wrapped 1×1 Kitty graphics query for explicit `tmux` passthrough mode.
pub const KITTY_TMUX_PASSTHROUGH_QUERY: &[u8] =
    b"\x1bPtmux;\x1b\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\x1b\\\x1b\\";

/// DCS-wrapped 1×1 Kitty zlib-compression query for explicit `tmux` passthrough mode.
pub const KITTY_TMUX_PASSTHROUGH_ZLIB_QUERY: &[u8] =
    b"\x1bPtmux;\x1b\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24,o=z;eJxjYGAAAAADAAE=\x1b\x1b\\\x1b\\";

/// DECRQM query for DEC synchronized output mode (`CSI ? 2026 $ p`).
pub const DECRQM_SYNC_OUTPUT_QUERY: &[u8] = b"\x1b[?2026$p";

/// DECRQM query for grapheme-cluster width mode (`CSI ? 2027 $ p`).
pub const DECRQM_GRAPHEME_QUERY: &[u8] = b"\x1b[?2027$p";

/// Kitty keyboard enhancement flags query (`CSI ? u`).
pub const KITTY_KEYBOARD_QUERY: &[u8] = b"\x1b[?u";

/// XTWINOPS query for character cell size in pixels (`CSI 16 t`).
pub const XTWINOPS_CELL_PX_QUERY: &[u8] = b"\x1b[16t";

/// XTWINOPS query for text area size in pixels (`CSI 14 t`).
pub const XTWINOPS_AREA_PX_QUERY: &[u8] = b"\x1b[14t";

/// Primary Device Attributes (`DA1`) fence query (`CSI c`).
pub const DA1_QUERY: &[u8] = b"\x1b[c";

/// Maximum CSI parameter payload accepted by [`ProbeResponseParser`] before aborting.
const MAX_CSI_LEN: usize = 128;

/// Maximum APC/DCS payload accepted by [`ProbeResponseParser`] before aborting.
const MAX_STRING_SEQ_LEN: usize = 256;

/// Maximum non-probe leftover bytes retained during a probe window.
const MAX_LEFTOVER_BYTES: usize = 256;

/// Source that provided the active [`ViewportGeometry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeometrySource {
    /// Explicit CLI `--geometry COLSxROWS` override.
    CliOverride,
    /// Kernel `ioctl(STDOUT_FILENO, TIOCGWINSZ)`.
    IoctlWinsize,
    /// VT100 cursor-clamp (`ESC 7 CSI 999;999H CSI 6n` -> `DSR` + `ESC 8`).
    Vt100CursorClamp,
    /// Safe `80×24` fallback when ioctl and DSR both failed or timed out.
    Fallback80x24,
}

/// Snapshot of terminal-relevant environment variables (pure value type for offline testing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalEnv {
    /// `$TERM`
    pub term: Option<String>,
    /// `$COLORTERM`
    pub colorterm: Option<String>,
    /// `$TERM_PROGRAM`
    pub term_program: Option<String>,
    /// `$KITTY_WINDOW_ID`
    pub kitty_window_id: Option<String>,
    /// `$WEZTERM_EXECUTABLE`
    pub wezterm_executable: Option<String>,
    /// `$KONSOLE_VERSION`
    pub konsole_version: Option<String>,
    /// `$WT_SESSION` (Windows Terminal)
    pub wt_session: Option<String>,
    /// `$LC_TERMINAL` (e.g. `iTerm2` forwarded over SSH)
    pub lc_terminal: Option<String>,
    /// `$TMUX`
    pub tmux: Option<String>,
    /// `$ZELLIJ` or `$ZELLIJ_SESSION_NAME`
    pub zellij: Option<String>,
    /// `$STY` (GNU screen)
    pub sty: Option<String>,
    /// `$SSH_CONNECTION`
    pub ssh_connection: Option<String>,
    /// `$SSH_TTY`
    pub ssh_tty: Option<String>,
    /// Whether `$NO_COLOR` is set and non-empty.
    pub no_color: bool,
    /// `$LC_ALL`
    pub lc_all: Option<String>,
    /// `$LC_CTYPE`
    pub lc_ctype: Option<String>,
    /// `$LANG`
    pub lang: Option<String>,
}

impl TerminalEnv {
    /// Capture terminal environment variables from the current process.
    #[must_use]
    pub fn from_process() -> Self {
        let non_empty = |key: &str| env::var(key).ok().filter(|v| !v.trim().is_empty());
        Self {
            term: non_empty("TERM"),
            colorterm: non_empty("COLORTERM"),
            term_program: non_empty("TERM_PROGRAM"),
            kitty_window_id: non_empty("KITTY_WINDOW_ID"),
            wezterm_executable: non_empty("WEZTERM_EXECUTABLE"),
            konsole_version: non_empty("KONSOLE_VERSION"),
            wt_session: non_empty("WT_SESSION"),
            lc_terminal: non_empty("LC_TERMINAL"),
            tmux: non_empty("TMUX"),
            zellij: non_empty("ZELLIJ").or_else(|| non_empty("ZELLIJ_SESSION_NAME")),
            sty: non_empty("STY"),
            ssh_connection: non_empty("SSH_CONNECTION"),
            ssh_tty: non_empty("SSH_TTY"),
            no_color: non_empty("NO_COLOR").is_some(),
            lc_all: non_empty("LC_ALL"),
            lc_ctype: non_empty("LC_CTYPE"),
            lang: non_empty("LANG"),
        }
    }

    /// Detect the wrapping terminal multiplexer, if any.
    #[must_use]
    pub fn detect_multiplexer(&self) -> MultiplexerKind {
        if self.tmux.as_deref().is_some_and(|v| !v.is_empty()) {
            return MultiplexerKind::Tmux;
        }
        if self.zellij.as_deref().is_some_and(|v| !v.is_empty()) {
            return MultiplexerKind::Zellij;
        }
        if self.sty.as_deref().is_some_and(|v| !v.is_empty()) {
            return MultiplexerKind::Screen;
        }
        if let Some(term) = self.term.as_deref() {
            let lower = term.to_ascii_lowercase();
            if lower.starts_with("tmux") {
                return MultiplexerKind::Tmux;
            }
        }
        MultiplexerKind::None
    }

    /// Returns `true` when running inside an SSH session (`$SSH_CONNECTION` or `$SSH_TTY`).
    #[must_use]
    pub fn is_over_ssh(&self) -> bool {
        self.ssh_connection.as_deref().is_some_and(|v| !v.is_empty())
            || self.ssh_tty.as_deref().is_some_and(|v| !v.is_empty())
    }

    /// Returns `true` when `$TERM` indicates a dumb/non-ANSI terminal.
    #[must_use]
    pub fn is_dumb_term(&self) -> bool {
        self.term
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("dumb"))
    }

    /// Returns `true` when `$TERM` is the Linux kernel console VT (`TERM=linux`).
    #[must_use]
    pub fn is_linux_vt(&self) -> bool {
        self.term
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("linux"))
    }

    /// Returns `true` if locale variables indicate UTF-8 support.
    #[must_use]
    pub fn locale_is_utf8(&self) -> bool {
        let locale = self
            .lc_all
            .as_deref()
            .or(self.lc_ctype.as_deref())
            .or(self.lang.as_deref());
        match locale {
            Some(loc) => {
                let lower = loc.to_ascii_lowercase();
                lower.contains("utf-8") || lower.contains("utf8")
            }
            None => false,
        }
    }

    /// Infer terminal [`ColorDepth`] from environment variables alone.
    #[must_use]
    pub fn detect_color_depth(&self) -> ColorDepth {
        if self.no_color || self.is_dumb_term() {
            return ColorDepth::Monochrome;
        }

        if let Some(ct) = self.colorterm.as_deref() {
            let lower = ct.to_ascii_lowercase();
            if lower == "truecolor" || lower == "24bit" {
                return ColorDepth::TrueColor;
            }
        }

        if let Some(term) = self.term.as_deref() {
            let lower = term.to_ascii_lowercase();
            if lower.ends_with("-direct")
                || lower == "xterm-kitty"
                || lower == "xterm-ghostty"
                || lower.starts_with("foot")
                || lower == "wezterm"
                || lower == "alacritty"
                || lower == "contour"
                || lower == "rio"
            {
                return ColorDepth::TrueColor;
            }
        }

        if let Some(prog) = self.term_program.as_deref() {
            let lower = prog.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "wezterm"
                    | "iterm.app"
                    | "ghostty"
                    | "kitty"
                    | "rio"
                    | "vscode"
                    | "tabby"
                    | "hyper"
                    | "contour"
            ) {
                return ColorDepth::TrueColor;
            }
        }

        if let Some(term) = self.term.as_deref() {
            let lower = term.to_ascii_lowercase();
            if lower.contains("256color") {
                return ColorDepth::Ansi256;
            }
        }

        ColorDepth::Ansi16
    }

    /// Returns `true` if env identifies a terminal with Kitty Unicode placeholder (`U+10EEEE`) support.
    #[must_use]
    pub fn supports_kitty_placeholders_hint(&self) -> bool {
        let term = self.term.as_deref().unwrap_or_default().to_ascii_lowercase();
        let prog = self
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();

        term == "xterm-kitty"
            || term == "xterm-ghostty"
            || term == "rio"
            || prog == "kitty"
            || prog == "ghostty"
            || prog == "rio"
            || self.kitty_window_id.as_deref().is_some_and(|v| !v.is_empty())
    }

    /// Returns `true` if env identifies real `kitty` (supporting server-side `a=f`/`a=a` animation).
    #[must_use]
    pub fn is_native_kitty(&self) -> bool {
        let term = self.term.as_deref().unwrap_or_default().to_ascii_lowercase();
        let prog = self
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        term == "xterm-kitty"
            || prog == "kitty"
            || self.kitty_window_id.as_deref().is_some_and(|v| !v.is_empty())
    }

    /// Returns `true` if env identifies WezTerm or Konsole (Kitty direct without Unicode placeholders).
    #[must_use]
    pub fn is_kitty_direct_only_terminal(&self) -> bool {
        let term = self.term.as_deref().unwrap_or_default().to_ascii_lowercase();
        let prog = self
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();

        term == "wezterm"
            || prog == "wezterm"
            || prog == "konsole"
            || self
                .wezterm_executable
                .as_deref()
                .is_some_and(|v| !v.is_empty())
            || self
                .konsole_version
                .as_deref()
                .is_some_and(|v| !v.is_empty())
    }

    /// Returns `true` if env identifies an iTerm2 `OSC 1337` inline-image terminal.
    #[must_use]
    pub fn is_iterm2_terminal(&self) -> bool {
        let prog = self
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let lc_term = self
            .lc_terminal
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        matches!(prog.as_str(), "iterm.app" | "tabby" | "hyper") || lc_term == "iterm2"
    }
}

/// Configuration options controlling terminal probing and capability negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOptions {
    /// Explicit CLI `--geometry COLSxROWS` override.
    pub geometry_override: Option<ViewportGeometry>,
    /// Charset override (`Auto`, `Utf8`, or `Ascii7Bit`).
    pub charset_override: CharsetMode,
    /// Optional explicit graphics tier override.
    pub graphics_override: Option<GraphicsTier>,
    /// Optional explicit color depth override.
    pub color_depth_override: Option<ColorDepth>,
    /// Opt-in explicit `tmux` DCS passthrough wrapping for graphics protocols.
    pub tmux_passthrough: bool,
    /// Disable all active terminal escape-sequence probes (`--no-probe`).
    pub skip_active_probe: bool,
    /// Hard deadline for all active probes (default: `150 ms`).
    pub probe_timeout: Duration,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            geometry_override: None,
            charset_override: CharsetMode::Auto,
            graphics_override: None,
            color_depth_override: None,
            tmux_passthrough: false,
            skip_active_probe: false,
            probe_timeout: DEFAULT_PROBE_DEADLINE,
        }
    }
}

/// Parse a CLI `--geometry COLSxROWS` flag (e.g. `"16x2"`, `"80X24"`, `"640x180"`).
///
/// Rejects zero dimensions, missing separators, trailing garbage, or out-of-range `u16` values.
pub fn parse_geometry_override(raw: &str) -> TermResult<ViewportGeometry> {
    let trimmed = raw.trim();
    let sep_idx = trimmed
        .find(['x', 'X'])
        .ok_or_else(|| TermError::InvalidGeometry(trimmed.to_owned()))?;

    let cols_str = &trimmed[..sep_idx];
    let rows_str = &trimmed[sep_idx + 1..];

    let cols = cols_str
        .parse::<u16>()
        .map_err(|_| TermError::InvalidGeometry(trimmed.to_owned()))?;
    let rows = rows_str
        .parse::<u16>()
        .map_err(|_| TermError::InvalidGeometry(trimmed.to_owned()))?;

    if cols == 0 || rows == 0 {
        return Err(TermError::InvalidGeometry(trimmed.to_owned()));
    }

    Ok(ViewportGeometry::new(cols, rows))
}

/// Parsed results collected from terminal escape-sequence responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeResponse {
    /// Geometry reported by VT100 cursor-clamp DSR (`\x1b[{rows};{cols}R`), stored as `(cols, rows)`.
    pub dsr_geometry: Option<ViewportGeometry>,
    /// Kitty graphics protocol 1×1 query (`i=31`) returned `OK`.
    pub kitty_graphics: bool,
    /// Kitty graphics protocol 1×1 `o=z` zlib-compression query (`i=32`) returned `OK`.
    pub kitty_zlib_compression: bool,
    /// Primary Device Attributes (`DA1`) fence reply was received.
    pub da1_received: bool,
    /// `DA1` advertised attribute `4` (DEC Sixel graphics).
    pub sixel: bool,
    /// `DECRQM ?2026$p` reported synchronized output support.
    pub synchronized_output: bool,
    /// `DECRQM ?2027$p` reported grapheme-cluster width mode support.
    pub grapheme_mode_2027: bool,
    /// Kitty keyboard flags reported by `CSI ? u`.
    pub kitty_keyboard_flags: Option<u8>,
    /// Character cell dimensions `(width_px, height_px)` reported by `CSI 16t`.
    pub cell_px: Option<(u16, u16)>,
    /// Text-area dimensions `(width_px, height_px)` reported by `CSI 14t`.
    pub window_px: Option<(u16, u16)>,
    /// Non-probe bytes encountered while draining responses (bounded).
    pub leftover_bytes: Vec<u8>,
}

/// Incremental, zero-panic byte-stream parser for DSR, DA1, Kitty APC, DECRQM, and XTWINOPS replies.
#[derive(Debug, Clone, Default)]
pub struct ProbeResponseParser {
    state: ParserState,
    buf: Vec<u8>,
    response: ProbeResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ParserState {
    #[default]
    Ground,
    Esc,
    Csi,
    Apc,
    ApcEsc,
    Dcs,
    DcsEsc,
}

impl ProbeResponseParser {
    /// Create a fresh parser with empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a complete byte slice in one call.
    #[must_use]
    pub fn parse_slice(bytes: &[u8]) -> ProbeResponse {
        let mut parser = Self::new();
        parser.feed(bytes);
        parser.into_response()
    }

    /// Feed an arbitrary chunk of terminal response bytes (may be partial, noisy, or malformed).
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed_byte(byte);
        }
    }

    /// Borrow the accumulated parsed response so far.
    #[must_use]
    pub fn response(&self) -> &ProbeResponse {
        &self.response
    }

    /// Consume the parser and return the final [`ProbeResponse`].
    #[must_use]
    pub fn into_response(self) -> ProbeResponse {
        self.response
    }

    fn push_leftover(&mut self, byte: u8) {
        if self.response.leftover_bytes.len() < MAX_LEFTOVER_BYTES {
            self.response.leftover_bytes.push(byte);
        }
    }

    fn feed_byte(&mut self, byte: u8) {
        match self.state {
            ParserState::Ground => {
                if byte == 0x1B {
                    self.state = ParserState::Esc;
                } else {
                    self.push_leftover(byte);
                }
            }
            ParserState::Esc => match byte {
                b'[' => {
                    self.buf.clear();
                    self.state = ParserState::Csi;
                }
                b'_' => {
                    self.buf.clear();
                    self.state = ParserState::Apc;
                }
                b'P' => {
                    self.buf.clear();
                    self.state = ParserState::Dcs;
                }
                0x1B => {
                    self.state = ParserState::Esc;
                }
                _ => {
                    self.state = ParserState::Ground;
                }
            },
            ParserState::Csi => {
                if byte == 0x1B {
                    self.buf.clear();
                    self.state = ParserState::Esc;
                } else if (0x40..=0x7E).contains(&byte) {
                    self.handle_csi(byte);
                    self.buf.clear();
                    self.state = ParserState::Ground;
                } else if (0x20..=0x3F).contains(&byte) {
                    if self.buf.len() < MAX_CSI_LEN {
                        self.buf.push(byte);
                    } else {
                        self.buf.clear();
                        self.state = ParserState::Ground;
                    }
                } else {
                    // Malformed control/high byte in CSI: abort cleanly.
                    self.buf.clear();
                    self.state = ParserState::Ground;
                }
            }
            ParserState::Apc => {
                if byte == 0x1B {
                    self.state = ParserState::ApcEsc;
                } else if byte == 0x07 {
                    self.handle_apc();
                    self.buf.clear();
                    self.state = ParserState::Ground;
                } else if self.buf.len() < MAX_STRING_SEQ_LEN {
                    self.buf.push(byte);
                } else {
                    self.buf.clear();
                    self.state = ParserState::Ground;
                }
            }
            ParserState::ApcEsc => {
                if byte == b'\\' {
                    self.handle_apc();
                    self.buf.clear();
                    self.state = ParserState::Ground;
                } else if byte == b'[' {
                    // Interrupted APC followed immediately by CSI.
                    self.buf.clear();
                    self.state = ParserState::Csi;
                } else if byte == b'_' {
                    self.buf.clear();
                    self.state = ParserState::Apc;
                } else if byte == 0x1B {
                    self.buf.clear();
                    self.state = ParserState::Esc;
                } else {
                    self.buf.clear();
                    self.state = ParserState::Ground;
                }
            }
            ParserState::Dcs => {
                if byte == 0x1B {
                    self.state = ParserState::DcsEsc;
                } else if byte == 0x07 {
                    self.buf.clear();
                    self.state = ParserState::Ground;
                } else if self.buf.len() < MAX_STRING_SEQ_LEN {
                    self.buf.push(byte);
                } else {
                    self.buf.clear();
                    self.state = ParserState::Ground;
                }
            }
            ParserState::DcsEsc => {
                if byte == b'\\' {
                    self.buf.clear();
                    self.state = ParserState::Ground;
                } else if byte == 0x1B {
                    self.buf.clear();
                    self.state = ParserState::Esc;
                } else {
                    self.buf.clear();
                    self.state = ParserState::Ground;
                }
            }
        }
    }

    fn handle_csi(&mut self, final_byte: u8) {
        let Ok(params) = std::str::from_utf8(&self.buf) else {
            return;
        };

        match final_byte {
            // DSR Cursor Position Report: CSI rows ; cols R
            b'R' => {
                if params.starts_with('?') {
                    return;
                }
                let mut parts = params.split(';');
                let Some(rows_str) = parts.next() else {
                    return;
                };
                let Some(cols_str) = parts.next() else {
                    return;
                };
                if parts.next().is_some() {
                    return;
                }
                if let (Ok(rows), Ok(cols)) = (rows_str.parse::<u16>(), cols_str.parse::<u16>())
                    && rows > 0
                    && cols > 0
                {
                    self.response.dsr_geometry = Some(ViewportGeometry::new(cols, rows));
                }
            }
            // Primary Device Attributes (DA1): CSI ? p1 ; p2 ; ... c
            b'c' => {
                // Ignore DA2 (`CSI > ... c`) and DA3 (`CSI = ... c`).
                if params.starts_with('>') || params.starts_with('=') {
                    return;
                }
                self.response.da1_received = true;
                let body = params.strip_prefix('?').unwrap_or(params);
                // First parameter is terminal conformance class (e.g. 62, 63, 64, 65, 1);
                // remaining parameters are feature flags where exact `4` means DEC Sixel.
                // Some minimal terminals report `?4c` directly, so check every token after
                // handling both `?62;4c` and `?4c` (note: conformance class 4 does not exist).
                for token in body.split(';') {
                    if token.parse::<u16>() == Ok(4) {
                        self.response.sixel = true;
                    }
                }
            }
            // DECRQM reply: CSI ? mode ; status $ y
            b'y' => {
                let Some(without_dollar) = params.strip_suffix('$') else {
                    return;
                };
                let Some(body) = without_dollar.strip_prefix('?') else {
                    return;
                };
                let mut parts = body.split(';');
                let Some(mode_str) = parts.next() else {
                    return;
                };
                let Some(status_str) = parts.next() else {
                    return;
                };
                if parts.next().is_some() {
                    return;
                }
                let (Ok(mode), Ok(status)) = (mode_str.parse::<u16>(), status_str.parse::<u8>())
                else {
                    return;
                };
                // Ps=1 (set), Ps=2 (reset), Ps=3 (permanently set) indicate support.
                let supported = matches!(status, 1..=3);
                match mode {
                    2026 => self.response.synchronized_output = supported,
                    2027 => self.response.grapheme_mode_2027 = supported,
                    _ => {}
                }
            }
            // Kitty keyboard flags reply: CSI ? flags u
            b'u' => {
                if let Some(body) = params.strip_prefix('?')
                    && let Ok(flags) = body.parse::<u8>()
                {
                    self.response.kitty_keyboard_flags = Some(flags);
                }
            }
            // XTWINOPS replies:
            // - CSI 6 ; cell_height ; cell_width t
            // - CSI 4 ; area_height ; area_width t
            b't' => {
                let mut parts = params.split(';');
                let Some(kind) = parts.next() else {
                    return;
                };
                let Some(h_str) = parts.next() else {
                    return;
                };
                let Some(w_str) = parts.next() else {
                    return;
                };
                if parts.next().is_some() {
                    return;
                }
                let (Ok(h), Ok(w)) = (h_str.parse::<u16>(), w_str.parse::<u16>()) else {
                    return;
                };
                if w == 0 || h == 0 {
                    return;
                }
                match kind {
                    "6" => self.response.cell_px = Some((w, h)),
                    "4" => self.response.window_px = Some((w, h)),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_apc(&mut self) {
        let Ok(payload) = std::str::from_utf8(&self.buf) else {
            return;
        };
        let Some(kitty_body) = payload.strip_prefix('G') else {
            return;
        };
        let Some((header, status)) = kitty_body.split_once(';') else {
            return;
        };
        if status.trim() != "OK" {
            return;
        }
        // Match query image ID `i=31` (base Kitty support) or `i=32` (`o=z` zlib support).
        if header.split(',').any(|kv| kv.trim() == "i=32") {
            self.response.kitty_zlib_compression = true;
            return;
        }
        let id_matches = header.is_empty() || header.split(',').any(|kv| kv.trim() == "i=31");
        if id_matches {
            self.response.kitty_graphics = true;
        }
    }
}

/// Complete capability negotiation report produced by [`negotiate_capabilities`] or [`probe_terminal`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReport {
    /// Core terminal capabilities contract shared across `bbscord`.
    pub capabilities: TerminalCapabilities,
    /// Concrete resolved charset (`Utf8` vs `Ascii7Bit`).
    pub resolved_charset: ResolvedCharset,
    /// `true` on Linux virtual console (`TERM=linux`) where 256/512-glyph PSF fonts lack Braille.
    pub vt_safe_charset: bool,
    /// Adaptive `ESC` disambiguation timeout in milliseconds (`50` local, `100` SSH, `0` Kitty kbd).
    pub esc_timeout_ms: u16,
    /// Resolved character cell dimensions `(width_px, height_px)` (always `>= (1, 1)`).
    pub cell_pixels: (u16, u16),
    /// Resolved character cell pixel aspect ratio (`cell_w / cell_h`, `0.5` fallback).
    pub cell_aspect_ratio: f32,
    /// Whether `DECRQM ?2027$p` confirmed grapheme-cluster width mode.
    pub grapheme_mode_2027: bool,
    /// Whether Kitty graphics query succeeded.
    pub kitty_graphics_supported: bool,
    /// Whether Kitty graphics `o=z` zlib compression probe (`i=32`) succeeded.
    pub kitty_zlib_compression: bool,
    /// Whether terminal-side Kitty animation frames (`a=f`/`a=a`) are supported (native `kitty`).
    pub kitty_animation_supported: bool,
    /// Whether `DA1` advertised DEC Sixel (`;4`).
    pub sixel_supported: bool,
    /// Whether `tmux` DCS passthrough wrapping should be applied to graphics payloads.
    pub tmux_passthrough_active: bool,
    /// Origin of the negotiated [`ViewportGeometry`].
    pub geometry_source: GeometrySource,
    /// `true` if active probe hit the hard deadline before receiving `DA1`.
    pub probe_timed_out: bool,
    /// `true` if active probing was skipped (dumb/linux/micro/ASCII/non-TTY/`--no-probe`).
    pub probe_skipped: bool,
    /// Any non-probe bytes captured during the probe window.
    pub leftover_bytes: Vec<u8>,
}

/// Compute effective `(geometry_with_pixels, cell_pixels, cell_aspect_ratio)` combining
/// `TIOCGWINSZ`, `CSI 16t`, `CSI 14t`, and safe `1:2` (`8×16`) fallback.
#[must_use]
pub fn resolve_pixel_metrics(
    geometry: ViewportGeometry,
    probe: &ProbeResponse,
) -> (ViewportGeometry, (u16, u16), f32) {
    if let Some((cw, ch)) = geometry.cell_pixels() {
        return (geometry, (cw, ch), f32::from(cw) / f32::from(ch));
    }

    if let Some((cw, ch)) = probe.cell_px
        && cw > 0
        && ch > 0
    {
        let px_w = geometry.cols.saturating_mul(cw);
        let px_h = geometry.rows.saturating_mul(ch);
        let updated = ViewportGeometry::with_pixels(geometry.cols, geometry.rows, px_w, px_h);
        return (updated, (cw, ch), f32::from(cw) / f32::from(ch));
    }

    if let Some((area_w, area_h)) = probe.window_px
        && geometry.cols > 0
        && geometry.rows > 0
    {
        let cw = area_w / geometry.cols;
        let ch = area_h / geometry.rows;
        if cw > 0 && ch > 0 {
            let updated =
                ViewportGeometry::with_pixels(geometry.cols, geometry.rows, area_w, area_h);
            return (updated, (cw, ch), f32::from(cw) / f32::from(ch));
        }
    }

    (geometry, DEFAULT_CELL_PIXELS, DEFAULT_CELL_ASPECT_RATIO)
}

/// Compute adaptive ESC disambiguation timeout in milliseconds:
/// - `0 ms` when Kitty keyboard protocol (`CSI u`) is active
/// - `100 ms` over SSH (`$SSH_CONNECTION` / `$SSH_TTY`)
/// - `50 ms` local otherwise
#[must_use]
pub const fn compute_esc_timeout_ms(keyboard: KeyboardProtocol, over_ssh: bool) -> u16 {
    if matches!(keyboard, KeyboardProtocol::KittyCsiU) {
        ESC_TIMEOUT_KITTY_KBD_MS
    } else if over_ssh {
        ESC_TIMEOUT_SSH_MS
    } else {
        ESC_TIMEOUT_LOCAL_MS
    }
}

/// Pure, deterministic capability & graphics-tier negotiator.
#[must_use]
pub fn negotiate_capabilities(
    env: &TerminalEnv,
    raw_geometry: ViewportGeometry,
    geometry_source: GeometrySource,
    probe: &ProbeResponse,
    options: &ProbeOptions,
    probe_timed_out: bool,
    probe_skipped: bool,
) -> ProbeReport {
    let multiplexer = env.detect_multiplexer();
    let over_ssh = env.is_over_ssh();

    let utf8_supported = !env.is_dumb_term() && env.locale_is_utf8();
    let resolved_charset = options.charset_override.resolve(utf8_supported);
    let vt_safe_charset = resolved_charset == ResolvedCharset::Utf8 && env.is_linux_vt();

    let color_depth = options
        .color_depth_override
        .unwrap_or_else(|| env.detect_color_depth());

    let (geometry, cell_pixels, cell_aspect_ratio) = resolve_pixel_metrics(raw_geometry, probe);

    let is_micro = matches!(geometry.layout_profile(), LayoutProfile::Micro16x2);

    let keyboard = if probe.kitty_keyboard_flags.is_some()
        && !env.is_dumb_term()
        && !env.is_linux_vt()
        && !is_micro
    {
        KeyboardProtocol::KittyCsiU
    } else {
        KeyboardProtocol::LegacyVt
    };

    let esc_timeout_ms = compute_esc_timeout_ms(keyboard, over_ssh);

    let tmux_passthrough_active =
        multiplexer == MultiplexerKind::Tmux && options.tmux_passthrough;

    let graphics_tier = select_graphics_tier(
        env,
        geometry,
        resolved_charset,
        color_depth,
        multiplexer,
        probe,
        options,
        tmux_passthrough_active,
    );

    let kitty_animation_supported = matches!(graphics_tier, GraphicsTier::KittyPlaceholder)
        && env.is_native_kitty()
        && !over_ssh;

    let osc52_clipboard = !env.is_dumb_term()
        && !env.is_linux_vt()
        && resolved_charset != ResolvedCharset::Ascii7Bit;

    // Mutate from Default so Wave 0.5 additions to `TerminalCapabilities` merge cleanly.
    #[allow(clippy::field_reassign_with_default)]
    let capabilities = {
        let mut caps = TerminalCapabilities::default();
        caps.geometry = geometry;
        caps.graphics_tier = graphics_tier;
        caps.color_depth = color_depth;
        caps.charset = options.charset_override;
        caps.multiplexer = multiplexer;
        caps.keyboard = keyboard;
        caps.synchronized_output = probe.synchronized_output
            && !env.is_dumb_term()
            && resolved_charset != ResolvedCharset::Ascii7Bit;
        caps.bracketed_paste = !env.is_dumb_term();
        caps.mouse_sgr = !env.is_dumb_term() && !env.is_linux_vt() && !is_micro;
        caps.osc52_clipboard = osc52_clipboard;
        caps.over_ssh = over_ssh;
        caps
    };

    ProbeReport {
        capabilities,
        resolved_charset,
        vt_safe_charset,
        esc_timeout_ms,
        cell_pixels,
        cell_aspect_ratio,
        grapheme_mode_2027: probe.grapheme_mode_2027
            && !env.is_dumb_term()
            && resolved_charset == ResolvedCharset::Utf8,
        kitty_graphics_supported: probe.kitty_graphics,
        kitty_zlib_compression: probe.kitty_zlib_compression,
        kitty_animation_supported,
        sixel_supported: probe.sixel,
        tmux_passthrough_active,
        geometry_source,
        probe_timed_out,
        probe_skipped,
        leftover_bytes: probe.leftover_bytes.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn select_graphics_tier(
    env: &TerminalEnv,
    geometry: ViewportGeometry,
    resolved_charset: ResolvedCharset,
    color_depth: ColorDepth,
    multiplexer: MultiplexerKind,
    probe: &ProbeResponse,
    options: &ProbeOptions,
    tmux_passthrough_active: bool,
) -> GraphicsTier {
    // Hard safety floor: tiny LCD/micro panes and 7-bit ASCII ALWAYS force TextTagOnly.
    if geometry.rows <= 4
        || geometry.cols < 24
        || resolved_charset == ResolvedCharset::Ascii7Bit
    {
        return GraphicsTier::TextTagOnly;
    }

    if let Some(forced) = options.graphics_override {
        return forced;
    }

    if color_depth == ColorDepth::Monochrome || env.is_dumb_term() || env.is_linux_vt() {
        return GraphicsTier::TextTagOnly;
    }

    // Inside multiplexers (`$TMUX`, `$ZELLIJ`, `$STY`), default to HalfBlock so pane splits
    // and scrolling never leave ghost pixel artifacts unless explicit tmux passthrough is on.
    if multiplexer != MultiplexerKind::None && !tmux_passthrough_active {
        return GraphicsTier::HalfBlock;
    }

    if probe.kitty_graphics {
        if env.is_kitty_direct_only_terminal() {
            return GraphicsTier::KittyDirect;
        }
        if env.supports_kitty_placeholders_hint() {
            return GraphicsTier::KittyPlaceholder;
        }
        // Conservative default when Kitty answered `a=q` on an unrecognized terminal whose
        // Unicode placeholder (`U+10EEEE`) support is unconfirmed.
        return GraphicsTier::KittyDirect;
    }

    if probe.sixel {
        return GraphicsTier::Sixel;
    }

    if env.is_iterm2_terminal() {
        return GraphicsTier::ITerm2;
    }

    GraphicsTier::HalfBlock
}

/// Build the single batched active probe payload written to the terminal.
#[must_use]
pub fn build_active_probe_payload(
    include_tmux_passthrough_kitty: bool,
    query_pixels: bool,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(256);
    if include_tmux_passthrough_kitty {
        payload.extend_from_slice(KITTY_TMUX_PASSTHROUGH_QUERY);
        payload.extend_from_slice(KITTY_TMUX_PASSTHROUGH_ZLIB_QUERY);
    } else {
        payload.extend_from_slice(KITTY_GRAPHICS_QUERY);
        payload.extend_from_slice(KITTY_ZLIB_QUERY);
    }
    payload.extend_from_slice(DECRQM_SYNC_OUTPUT_QUERY);
    payload.extend_from_slice(DECRQM_GRAPHEME_QUERY);
    payload.extend_from_slice(KITTY_KEYBOARD_QUERY);
    if query_pixels {
        payload.extend_from_slice(XTWINOPS_CELL_PX_QUERY);
        payload.extend_from_slice(XTWINOPS_AREA_PX_QUERY);
    }
    payload.extend_from_slice(DA1_QUERY);
    payload
}

/// Query terminal geometry via kernel `ioctl(STDOUT_FILENO, TIOCGWINSZ)` on Unix.
#[cfg(unix)]
#[must_use]
pub fn query_stdout_winsize() -> Option<ViewportGeometry> {
    query_winsize_fd(libc::STDOUT_FILENO)
}

/// Fallback stub on non-Unix platforms.
#[cfg(not(unix))]
#[must_use]
pub fn query_stdout_winsize() -> Option<ViewportGeometry> {
    crossterm::terminal::size()
        .ok()
        .filter(|&(c, r)| c > 0 && r > 0)
        .map(|(c, r)| ViewportGeometry::new(c, r))
}

/// Query terminal geometry via kernel `ioctl(fd, TIOCGWINSZ)` on Unix.
#[cfg(unix)]
#[must_use]
pub fn query_winsize_fd(fd: std::os::fd::RawFd) -> Option<ViewportGeometry> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCGWINSZ` writes a `libc::winsize` struct into the valid pointer `&mut ws`.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some(ViewportGeometry::with_pixels(
            ws.ws_col,
            ws.ws_row,
            ws.ws_xpixel,
            ws.ws_ypixel,
        ))
    } else {
        None
    }
}

/// Probe the live attached terminal (`STDIN_FILENO`/`STDOUT_FILENO`) under a hard deadline.
///
/// This function performs bounded synchronous TTY I/O and should be invoked either before
/// starting the Tokio event loop or via [`probe_terminal_async`].
pub fn probe_terminal(options: &ProbeOptions) -> ProbeReport {
    let env = TerminalEnv::from_process();
    #[cfg(unix)]
    {
        probe_fd_pair(libc::STDIN_FILENO, libc::STDOUT_FILENO, &env, options)
    }
    #[cfg(not(unix))]
    {
        let (geom, src) = if let Some(ov) = options.geometry_override {
            (ov, GeometrySource::CliOverride)
        } else if let Some(ws) = query_stdout_winsize() {
            (ws, GeometrySource::IoctlWinsize)
        } else {
            (ViewportGeometry::default(), GeometrySource::Fallback80x24)
        };
        negotiate_capabilities(
            &env,
            geom,
            src,
            &ProbeResponse::default(),
            options,
            false,
            true,
        )
    }
}

/// Run [`probe_terminal`] on Tokio's blocking thread pool so async worker threads are never blocked.
pub async fn probe_terminal_async(options: ProbeOptions) -> TermResult<ProbeReport> {
    tokio::task::spawn_blocking(move || probe_terminal(&options))
        .await
        .map_err(|err| TermError::TaskJoin(err.to_string()))
}

#[cfg(unix)]
struct TempRawModeGuard {
    fd: std::os::fd::RawFd,
    saved: libc::termios,
}

#[cfg(unix)]
impl TempRawModeGuard {
    fn enter(fd: std::os::fd::RawFd) -> io::Result<Self> {
        let mut saved = std::mem::MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: `tcgetattr` initializes `saved` on success for a valid TTY fd.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `tcgetattr` returned 0, so `saved` is fully initialized.
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a valid modified copy of `saved`.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, saved })
    }
}

#[cfg(unix)]
impl Drop for TempRawModeGuard {
    fn drop(&mut self) {
        // SAFETY: Restores the exact `termios` previously captured from `self.fd`.
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

#[cfg(unix)]
fn is_tty_fd(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: `isatty` is safe for any file descriptor integer.
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(unix)]
fn write_all_fd(fd: std::os::fd::RawFd, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data.as_ptr()` is valid for `data.len()` bytes.
        let written = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if written < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short write to terminal fd",
            ));
        }
        let n = written as usize;
        data = &data[n..];
    }
    Ok(())
}

#[cfg(unix)]
fn poll_and_feed(
    stdin_fd: std::os::fd::RawFd,
    parser: &mut ProbeResponseParser,
    deadline: Instant,
    stop_when: impl Fn(&ProbeResponse) -> bool,
) -> bool {
    let mut buf = [0u8; 256];
    loop {
        if stop_when(parser.response()) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        let rem_ms = deadline
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, i32::MAX as u128) as i32;

        let mut pfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` points to a single valid `libc::pollfd`.
        let ready = unsafe { libc::poll(&mut pfd, 1, rem_ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return true;
        }
        if ready == 0 {
            return true;
        }
        if (pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0
            && (pfd.revents & libc::POLLIN) == 0
        {
            return true;
        }
        if (pfd.revents & libc::POLLIN) != 0 {
            // SAFETY: `buf` is valid writable storage of `buf.len()` bytes.
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted || err.kind() == io::ErrorKind::WouldBlock
                {
                    continue;
                }
                return true;
            }
            if n == 0 {
                return true;
            }
            parser.feed(&buf[..n as usize]);
        }
    }
}

/// Probe a specific Unix `(stdin_fd, stdout_fd)` pair with a strict hard deadline.
///
/// Works on live TTYs as well as pipe/socketpair harnesses in tests.
#[cfg(unix)]
pub fn probe_fd_pair(
    stdin_fd: std::os::fd::RawFd,
    stdout_fd: std::os::fd::RawFd,
    env: &TerminalEnv,
    options: &ProbeOptions,
) -> ProbeReport {
    let is_tty = is_tty_fd(stdin_fd) && is_tty_fd(stdout_fd);
    probe_fd_pair_inner(stdin_fd, stdout_fd, is_tty, env, options)
}

#[cfg(unix)]
fn probe_fd_pair_inner(
    stdin_fd: std::os::fd::RawFd,
    stdout_fd: std::os::fd::RawFd,
    allow_active_io: bool,
    env: &TerminalEnv,
    options: &ProbeOptions,
) -> ProbeReport {
    let deadline = Instant::now() + options.probe_timeout;
    let _raw_guard = if allow_active_io && is_tty_fd(stdin_fd) {
        TempRawModeGuard::enter(stdin_fd).ok()
    } else {
        None
    };

    let mut parser = ProbeResponseParser::new();
    let mut timed_out = false;
    let mut probe_skipped = false;

    // 1. Geometry cascade: CLI override -> TIOCGWINSZ -> VT100 cursor-clamp DSR -> 80x24 fallback.
    let (geometry, geometry_source) = if let Some(override_geom) = options.geometry_override {
        (override_geom, GeometrySource::CliOverride)
    } else if let Some(ioctl_geom) = query_winsize_fd(stdout_fd) {
        (ioctl_geom, GeometrySource::IoctlWinsize)
    } else if allow_active_io && !options.skip_active_probe && !env.is_dumb_term() {
        let mut dsr_geom = None;
        if write_all_fd(stdout_fd, VT100_CURSOR_CLAMP_QUERY).is_ok() {
            let dsr_timed_out = poll_and_feed(stdin_fd, &mut parser, deadline, |resp| {
                resp.dsr_geometry.is_some()
            });
            // Always restore cursor position even on timeout.
            let _ = write_all_fd(stdout_fd, VT100_CURSOR_RESTORE);
            if dsr_timed_out {
                timed_out = true;
            }
            dsr_geom = parser.response().dsr_geometry;
        }
        match dsr_geom {
            Some(geom) => (geom, GeometrySource::Vt100CursorClamp),
            None => (ViewportGeometry::default(), GeometrySource::Fallback80x24),
        }
    } else {
        (ViewportGeometry::default(), GeometrySource::Fallback80x24)
    };

    // 2. Active graphics & terminal mode probe (skipped on dumb/linux/micro/ASCII/non-TTY).
    let utf8_supported = !env.is_dumb_term() && env.locale_is_utf8();
    let resolved_charset = options.charset_override.resolve(utf8_supported);
    let should_skip = options.skip_active_probe
        || !allow_active_io
        || env.is_dumb_term()
        || env.is_linux_vt()
        || resolved_charset == ResolvedCharset::Ascii7Bit
        || geometry.rows <= 4
        || geometry.cols < 24
        || Instant::now() >= deadline;

    if should_skip {
        probe_skipped = true;
    } else {
        let mux = env.detect_multiplexer();
        let tmux_passthrough = mux == MultiplexerKind::Tmux && options.tmux_passthrough;
        let query_pixels = !geometry.has_pixel_metrics();
        let payload = build_active_probe_payload(tmux_passthrough, query_pixels);
        if write_all_fd(stdout_fd, &payload).is_ok() {
            let active_timed_out = poll_and_feed(stdin_fd, &mut parser, deadline, |resp| {
                if tmux_passthrough {
                    // Under tmux passthrough, tmux answers DA1 before the outer terminal answers
                    // the DCS-wrapped Kitty query, so wait until Kitty also answers or deadline hits.
                    resp.da1_received && resp.kitty_graphics
                } else {
                    resp.da1_received
                }
            });
            if active_timed_out && !parser.response().da1_received {
                timed_out = true;
            }
        }
    }

    let response = parser.into_response();
    negotiate_capabilities(
        env,
        geometry,
        geometry_source,
        &response,
        options,
        timed_out,
        probe_skipped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8_env() -> TerminalEnv {
        TerminalEnv {
            lang: Some("en_US.UTF-8".to_owned()),
            ..TerminalEnv::default()
        }
    }

    #[test]
    fn parse_geometry_override_valid_and_invalid() {
        assert_eq!(
            parse_geometry_override("16x2").ok(),
            Some(ViewportGeometry::new(16, 2))
        );
        assert_eq!(
            parse_geometry_override("  640X180 ").ok(),
            Some(ViewportGeometry::new(640, 180))
        );
        assert!(parse_geometry_override("0x24").is_err());
        assert!(parse_geometry_override("80x0").is_err());
        assert!(parse_geometry_override("80").is_err());
        assert!(parse_geometry_override("abcx24").is_err());
        assert!(parse_geometry_override("99999x24").is_err());
        assert!(parse_geometry_override("").is_err());
    }

    #[test]
    fn parser_handles_complete_dsr_kitty_decrqm_xtwinops_and_da1() {
        let stream = concat!(
            "\x1b[2;16R",
            "\x1b_Gi=31;OK\x1b\\",
            "\x1b[?2026;2$y",
            "\x1b[?2027;1$y",
            "\x1b[?1u",
            "\x1b[6;20;10t",
            "\x1b[4;1000;1600t",
            "\x1b[?62;1;2;4;6;22c",
        );
        let resp = ProbeResponseParser::parse_slice(stream.as_bytes());
        assert_eq!(resp.dsr_geometry, Some(ViewportGeometry::new(16, 2)));
        assert!(resp.kitty_graphics);
        assert!(resp.synchronized_output);
        assert!(resp.grapheme_mode_2027);
        assert_eq!(resp.kitty_keyboard_flags, Some(1));
        // CSI 16t reports height=20, width=10 -> stored as (width=10, height=20).
        assert_eq!(resp.cell_px, Some((10, 20)));
        // CSI 14t reports height=1000, width=1600 -> stored as (width=1600, height=1000).
        assert_eq!(resp.window_px, Some((1600, 1000)));
        assert!(resp.da1_received);
        assert!(resp.sixel);
    }

    #[test]
    fn parser_handles_byte_by_byte_fragmented_and_noisy_uart_bytes() {
        let mut parser = ProbeResponseParser::new();
        let noisy_chunks: &[&[u8]] = &[
            b"\x00\xffgarbage uart noise",
            b"\x1b[24",
            b";80R",
            b"\x1b[?99999999999999999999c",
            b"\x1b_Gi=31;EINVAL\x1b\\",
            b"\x1b_Gi=31,p=0;OK",
            b"\x1b\\",
            b"\x1b[>1;10;0c", // DA2 must NOT set da1_received
            b"\x1b[?64;14;42c", // Contains '4' inside 64/14/42, NOT standalone ;4!
        ];
        for chunk in noisy_chunks {
            for &b in *chunk {
                parser.feed(&[b]);
            }
        }
        let resp = parser.into_response();
        assert_eq!(resp.dsr_geometry, Some(ViewportGeometry::new(80, 24)));
        assert!(resp.kitty_graphics);
        assert!(resp.da1_received);
        assert!(
            !resp.sixel,
            "parameters 64, 14, 42 must NOT falsely trigger Sixel attribute 4"
        );
    }

    #[test]
    fn parser_rejects_zero_and_truncated_sequences_without_panicking() {
        let bad = b"\x1b[0;0R\x1b[6;0;10t\x1b[4;100;0t\x1b[?2026;0$y\x1b_Gi=99;OK\x1b\\\x1b[?abc";
        let resp = ProbeResponseParser::parse_slice(bad);
        assert_eq!(resp.dsr_geometry, None);
        assert_eq!(resp.cell_px, None);
        assert_eq!(resp.window_px, None);
        assert!(!resp.synchronized_output);
        assert!(!resp.kitty_graphics);
        assert!(!resp.da1_received);
    }

    #[test]
    fn pixel_aspect_ratio_precedence_and_fallback() {
        // 1. TIOCGWINSZ wins when non-zero.
        let geom_px = ViewportGeometry::with_pixels(100, 40, 900, 720);
        let (g, cell, ar) = resolve_pixel_metrics(geom_px, &ProbeResponse::default());
        assert_eq!(g, geom_px);
        assert_eq!(cell, (9, 18));
        assert!((ar - 0.5).abs() < 1e-5);

        // 2. CSI 16t fills in when TIOCGWINSZ has 0x0 pixels.
        let geom_no_px = ViewportGeometry::new(80, 24);
        let probe_16t = ProbeResponse {
            cell_px: Some((12, 20)),
            ..ProbeResponse::default()
        };
        let (g, cell, ar) = resolve_pixel_metrics(geom_no_px, &probe_16t);
        assert_eq!(cell, (12, 20));
        assert_eq!(g.pixel_width, 960);
        assert_eq!(g.pixel_height, 480);
        assert!((ar - 0.6).abs() < 1e-5);

        // 3. Fallback 1:2 (8x16) when both TIOCGWINSZ and XTWINOPS are absent.
        let (_, cell, ar) = resolve_pixel_metrics(geom_no_px, &ProbeResponse::default());
        assert_eq!(cell, DEFAULT_CELL_PIXELS);
        assert!((ar - 0.5).abs() < 1e-5);
    }

    #[test]
    fn esc_timeout_matrix_local_ssh_and_kitty_kbd() {
        assert_eq!(compute_esc_timeout_ms(KeyboardProtocol::LegacyVt, false), 50);
        assert_eq!(compute_esc_timeout_ms(KeyboardProtocol::LegacyVt, true), 100);
        assert_eq!(compute_esc_timeout_ms(KeyboardProtocol::KittyCsiU, false), 0);
        assert_eq!(compute_esc_timeout_ms(KeyboardProtocol::KittyCsiU, true), 0);
    }

    #[test]
    fn graphics_tier_matrix_all_terminals_and_multiplexers() {
        let geom = ViewportGeometry::new(120, 40);
        let opts = ProbeOptions::default();

        // 1. Micro 16x2 -> TextTagOnly even if Kitty responded OK.
        let kitty_ok = ProbeResponse {
            kitty_graphics: true,
            da1_received: true,
            ..ProbeResponse::default()
        };
        let mut env = utf8_env();
        env.term = Some("xterm-kitty".to_owned());
        let report = negotiate_capabilities(
            &env,
            ViewportGeometry::new(16, 2),
            GeometrySource::CliOverride,
            &kitty_ok,
            &opts,
            false,
            false,
        );
        assert_eq!(report.capabilities.graphics_tier, GraphicsTier::TextTagOnly);

        // 2. Ascii7Bit -> TextTagOnly even on desktop kitty.
        let ascii_opts = ProbeOptions {
            charset_override: CharsetMode::Ascii7Bit,
            ..ProbeOptions::default()
        };
        let report = negotiate_capabilities(
            &env,
            geom,
            GeometrySource::IoctlWinsize,
            &kitty_ok,
            &ascii_opts,
            false,
            false,
        );
        assert_eq!(report.capabilities.graphics_tier, GraphicsTier::TextTagOnly);

        // 3. kitty / ghostty / Rio -> KittyPlaceholder.
        for (term, prog, expect_anim) in [
            ("xterm-kitty", "", true),
            ("xterm-ghostty", "ghostty", false),
            ("rio", "Rio", false),
        ] {
            let mut e = utf8_env();
            e.term = Some(term.to_owned());
            if !prog.is_empty() {
                e.term_program = Some(prog.to_owned());
            }
            let r = negotiate_capabilities(
                &e,
                geom,
                GeometrySource::IoctlWinsize,
                &kitty_ok,
                &opts,
                false,
                false,
            );
            assert_eq!(
                r.capabilities.graphics_tier,
                GraphicsTier::KittyPlaceholder,
                "failed for {term}/{prog}"
            );
            assert_eq!(r.kitty_animation_supported, expect_anim);
        }

        // 4. WezTerm / Konsole with Kitty OK -> KittyDirect.
        for prog in ["WezTerm", "konsole"] {
            let mut e = utf8_env();
            e.term_program = Some(prog.to_owned());
            let r = negotiate_capabilities(
                &e,
                geom,
                GeometrySource::IoctlWinsize,
                &kitty_ok,
                &opts,
                false,
                false,
            );
            assert_eq!(
                r.capabilities.graphics_tier,
                GraphicsTier::KittyDirect,
                "failed for {prog}"
            );
        }

        // 5. foot / xterm / Contour / Windows Terminal / Konsole with DA1 ;4 -> Sixel.
        let sixel_ok = ProbeResponse {
            da1_received: true,
            sixel: true,
            ..ProbeResponse::default()
        };
        for term in ["foot", "xterm-256color", "contour"] {
            let mut e = utf8_env();
            e.term = Some(term.to_owned());
            let r = negotiate_capabilities(
                &e,
                geom,
                GeometrySource::IoctlWinsize,
                &sixel_ok,
                &opts,
                false,
                false,
            );
            assert_eq!(r.capabilities.graphics_tier, GraphicsTier::Sixel);
        }

        // 6. iTerm.app / Tabby / Hyper without Kitty/Sixel -> ITerm2.
        let plain_da1 = ProbeResponse {
            da1_received: true,
            ..ProbeResponse::default()
        };
        for prog in ["iTerm.app", "Tabby", "Hyper"] {
            let mut e = utf8_env();
            e.term_program = Some(prog.to_owned());
            let r = negotiate_capabilities(
                &e,
                geom,
                GeometrySource::IoctlWinsize,
                &plain_da1,
                &opts,
                false,
                false,
            );
            assert_eq!(r.capabilities.graphics_tier, GraphicsTier::ITerm2);
        }

        // 7. Inside $TMUX or $ZELLIJ without passthrough -> HalfBlock even if KITTY_WINDOW_ID leaked!
        let mut tmux_env = utf8_env();
        tmux_env.term = Some("tmux-256color".to_owned());
        tmux_env.tmux = Some("/tmp/tmux-1000/default,123,0".to_owned());
        tmux_env.kitty_window_id = Some("1".to_owned());
        let r = negotiate_capabilities(
            &tmux_env,
            geom,
            GeometrySource::IoctlWinsize,
            &kitty_ok,
            &opts,
            false,
            false,
        );
        assert_eq!(r.capabilities.multiplexer, MultiplexerKind::Tmux);
        assert_eq!(r.capabilities.graphics_tier, GraphicsTier::HalfBlock);

        // With explicit tmux_passthrough enabled + Kitty OK -> KittyPlaceholder.
        let pt_opts = ProbeOptions {
            tmux_passthrough: true,
            ..ProbeOptions::default()
        };
        let r_pt = negotiate_capabilities(
            &tmux_env,
            geom,
            GeometrySource::IoctlWinsize,
            &kitty_ok,
            &pt_opts,
            false,
            false,
        );
        assert!(r_pt.tmux_passthrough_active);
        assert_eq!(
            r_pt.capabilities.graphics_tier,
            GraphicsTier::KittyPlaceholder
        );

        let mut zellij_env = utf8_env();
        zellij_env.term = Some("xterm-256color".to_owned());
        zellij_env.zellij = Some("0".to_owned());
        let r_z = negotiate_capabilities(
            &zellij_env,
            geom,
            GeometrySource::IoctlWinsize,
            &sixel_ok,
            &opts,
            false,
            false,
        );
        assert_eq!(r_z.capabilities.multiplexer, MultiplexerKind::Zellij);
        assert_eq!(r_z.capabilities.graphics_tier, GraphicsTier::HalfBlock);

        // 8. SSH sets over_ssh and 100 ms ESC timeout.
        let mut ssh_env = utf8_env();
        ssh_env.term = Some("xterm-256color".to_owned());
        ssh_env.ssh_connection = Some("10.0.0.2 51234 10.0.0.1 22".to_owned());
        let r_ssh = negotiate_capabilities(
            &ssh_env,
            geom,
            GeometrySource::IoctlWinsize,
            &plain_da1,
            &opts,
            false,
            false,
        );
        assert!(r_ssh.capabilities.over_ssh);
        assert_eq!(r_ssh.esc_timeout_ms, 100);
        assert_eq!(r_ssh.capabilities.graphics_tier, GraphicsTier::HalfBlock);

        // 9. Kitty keyboard protocol over SSH reduces ESC timeout to 0 ms.
        let kitty_kbd_resp = ProbeResponse {
            da1_received: true,
            kitty_keyboard_flags: Some(1),
            window_px: Some((1200, 800)),
            ..ProbeResponse::default()
        };
        let r_kbd = negotiate_capabilities(
            &ssh_env,
            ViewportGeometry::new(100, 40),
            GeometrySource::IoctlWinsize,
            &kitty_kbd_resp,
            &opts,
            false,
            false,
        );
        assert_eq!(r_kbd.capabilities.keyboard, KeyboardProtocol::KittyCsiU);
        assert_eq!(r_kbd.esc_timeout_ms, 0);
        // CSI 14t (1200x800 over 100x40 cells -> 12x20 cell pixels).
        assert_eq!(r_kbd.cell_pixels, (12, 20));

        // 10. GNU screen ($STY) -> Screen multiplexer, HalfBlock; Windows Terminal + DA1 ;4 -> Sixel.
        let mut screen_env = utf8_env();
        screen_env.sty = Some("1234.pts-0.host".to_owned());
        let r_scr = negotiate_capabilities(
            &screen_env,
            geom,
            GeometrySource::IoctlWinsize,
            &kitty_ok,
            &opts,
            false,
            false,
        );
        assert_eq!(r_scr.capabilities.multiplexer, MultiplexerKind::Screen);
        assert_eq!(r_scr.capabilities.graphics_tier, GraphicsTier::HalfBlock);

        let mut wt_env = utf8_env();
        wt_env.wt_session = Some("guid-1234".to_owned());
        let r_wt = negotiate_capabilities(
            &wt_env,
            geom,
            GeometrySource::IoctlWinsize,
            &sixel_ok,
            &opts,
            false,
            false,
        );
        assert_eq!(r_wt.capabilities.graphics_tier, GraphicsTier::Sixel);

        // 11. TERM=linux, NO_COLOR, and TERM=dumb degrade cleanly to TextTagOnly.
        let mut linux_env = utf8_env();
        linux_env.term = Some("linux".to_owned());
        let r_linux = negotiate_capabilities(
            &linux_env,
            geom,
            GeometrySource::IoctlWinsize,
            &sixel_ok,
            &opts,
            false,
            true,
        );
        assert!(r_linux.vt_safe_charset);
        assert_eq!(r_linux.capabilities.graphics_tier, GraphicsTier::TextTagOnly);

        let mut nocolor_env = utf8_env();
        nocolor_env.no_color = true;
        let r_nc = negotiate_capabilities(
            &nocolor_env,
            geom,
            GeometrySource::IoctlWinsize,
            &kitty_ok,
            &opts,
            false,
            false,
        );
        assert_eq!(r_nc.capabilities.color_depth, ColorDepth::Monochrome);
        assert_eq!(r_nc.capabilities.graphics_tier, GraphicsTier::TextTagOnly);

        let mut dumb_env = utf8_env();
        dumb_env.term = Some("dumb".to_owned());
        let r_dumb = negotiate_capabilities(
            &dumb_env,
            geom,
            GeometrySource::IoctlWinsize,
            &ProbeResponse::default(),
            &opts,
            false,
            true,
        );
        assert_eq!(r_dumb.capabilities.graphics_tier, GraphicsTier::TextTagOnly);
        assert!(!r_dumb.capabilities.bracketed_paste);
    }

    #[cfg(unix)]
    #[test]
    fn silent_uart_pipe_respects_hard_deadline_and_falls_back_to_80x24() {
        use std::os::fd::{AsRawFd, FromRawFd};

        // Create two pipes: probe writes queries to out_w (ignored) and reads replies from in_r (silent).
        let mut in_fds = [0i32; 2];
        let mut out_fds = [0i32; 2];
        // SAFETY: Valid 2-int arrays for `pipe`.
        assert_eq!(unsafe { libc::pipe(in_fds.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(out_fds.as_mut_ptr()) }, 0);

        // Wrap in OwnedFd for guaranteed cleanup.
        // SAFETY: Fresh fds returned by `pipe`.
        let in_r = unsafe { std::os::fd::OwnedFd::from_raw_fd(in_fds[0]) };
        let _in_w = unsafe { std::os::fd::OwnedFd::from_raw_fd(in_fds[1]) };
        let _out_r = unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fds[0]) };
        let out_w = unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fds[1]) };

        let env = utf8_env();
        let opts = ProbeOptions {
            probe_timeout: Duration::from_millis(45),
            ..ProbeOptions::default()
        };

        let start = Instant::now();
        let report =
            probe_fd_pair_inner(in_r.as_raw_fd(), out_w.as_raw_fd(), true, &env, &opts);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(250),
            "silent UART probe took too long: {elapsed:?}"
        );
        assert!(report.probe_timed_out);
        assert_eq!(report.geometry_source, GeometrySource::Fallback80x24);
        assert_eq!(report.capabilities.geometry, ViewportGeometry::new(80, 24));
    }

    #[cfg(unix)]
    #[test]
    fn vt100_cursor_clamp_dsr_discovers_serial_16x2_lcd() {
        use std::os::fd::{AsRawFd, FromRawFd};

        let mut in_fds = [0i32; 2];
        let mut out_fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(in_fds.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(out_fds.as_mut_ptr()) }, 0);

        let in_r = unsafe { std::os::fd::OwnedFd::from_raw_fd(in_fds[0]) };
        let in_w = unsafe { std::os::fd::OwnedFd::from_raw_fd(in_fds[1]) };
        let out_r = unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fds[0]) };
        let out_w = unsafe { std::os::fd::OwnedFd::from_raw_fd(out_fds[1]) };

        // Pre-seed simulated VT100 serial 16x2 LCD DSR reply (`\x1b[2;16R`).
        assert!(write_all_fd(in_w.as_raw_fd(), b"\x1b[2;16R").is_ok());

        let env = utf8_env();
        let opts = ProbeOptions {
            probe_timeout: Duration::from_millis(150),
            ..ProbeOptions::default()
        };

        let report =
            probe_fd_pair_inner(in_r.as_raw_fd(), out_w.as_raw_fd(), true, &env, &opts);

        assert_eq!(report.geometry_source, GeometrySource::Vt100CursorClamp);
        assert_eq!(report.capabilities.geometry, ViewportGeometry::new(16, 2));
        assert_eq!(report.capabilities.graphics_tier, GraphicsTier::TextTagOnly);
        assert!(report.probe_skipped, "graphics probe must be skipped on 16x2");

        // Verify both cursor-clamp query and cursor restore ESC 8 were written.
        let mut written = [0u8; 64];
        let n = unsafe { libc::read(out_r.as_raw_fd(), written.as_mut_ptr().cast(), written.len()) };
        assert!(n > 0);
        let emitted = &written[..n as usize];
        assert!(emitted.starts_with(VT100_CURSOR_CLAMP_QUERY));
        assert!(emitted.ends_with(VT100_CURSOR_RESTORE));
    }

    #[test]
    fn parser_distinguishes_kitty_base_and_zlib_compression_probes() {
        let base_only = ProbeResponseParser::parse_slice(
            b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EINVAL:unsupported compression\x1b\\\x1b[?62;c",
        );
        assert!(base_only.kitty_graphics);
        assert!(!base_only.kitty_zlib_compression);
        assert!(base_only.da1_received);

        let with_zlib = ProbeResponseParser::parse_slice(
            b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;OK\x1b\\\x1b[?62;c",
        );
        assert!(with_zlib.kitty_graphics);
        assert!(with_zlib.kitty_zlib_compression);

        let payload = build_active_probe_payload(false, false);
        assert!(payload.windows(KITTY_GRAPHICS_QUERY.len()).any(|w| w == KITTY_GRAPHICS_QUERY));
        assert!(payload.windows(KITTY_ZLIB_QUERY.len()).any(|w| w == KITTY_ZLIB_QUERY));
    }
}
