//! Universal terminal probing, raw-mode/TTY safety, multi-tier graphics rendering,
//! animated GIF decoding/caching, and `SIGWINCH` resize debouncing for `bbscord`.

#![warn(missing_docs)]

pub use bbs_core;

pub mod animator;
pub mod media;
pub mod probe;
pub mod tty_guard;

use bbs_core::error::CoreError;
use thiserror::Error;

/// Unified error type for terminal runtime probing, TTY safety, and media decoding/encoding.
#[derive(Debug, Error)]
pub enum TermError {
    /// Terminal or file-descriptor I/O failure.
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid CLI `--geometry COLSxROWS` override string.
    #[error("invalid geometry override '{0}' (expected COLSxROWS with positive integers)")]
    InvalidGeometry(String),

    /// Image or animated GIF decoding error.
    #[error("media decode error: {0}")]
    MediaDecode(String),

    /// Terminal graphics protocol encoding error (`Kitty`, `Sixel`, `ITerm2`, `HalfBlock`).
    #[error("media encode error: {0}")]
    MediaEncode(String),

    /// Background Tokio blocking task failed or was cancelled.
    #[error("background task join error: {0}")]
    TaskJoin(String),
}

impl From<TermError> for CoreError {
    fn from(err: TermError) -> Self {
        match err {
            TermError::MediaDecode(msg) | TermError::MediaEncode(msg) => Self::Media(msg),
            other => Self::Internal(other.to_string()),
        }
    }
}

/// Convenience result alias using [`TermError`].
pub type TermResult<T> = Result<T, TermError>;

pub use animator::{
    DEFAULT_RESIZE_DEBOUNCE, DEFAULT_SCROLL_SETTLE, FALLBACK_LOCAL_MAX_FPS, FramePacer,
    GifAnimationPlayer, KITTY_LOCAL_MAX_FPS, ResizeDebouncer, ResizeUpdate, SIXEL_OR_SSH_MAX_FPS,
    ScrollProxyDebouncer, SettledResize,
};
pub use media::{
    CacheInsertOutcome, DEFAULT_DECODE_CONCURRENCY, DEFAULT_MEDIA_CACHE_BYTES, DecodeLimits,
    DecodedMedia, KittyDirectFrame, MediaDecoderPool, MediaFitMode, MediaFrameCache, MediaRenderer,
    MediaWidget, PtyBandwidthGovernor, RenderedMedia, TextTagFrame, aspect_clamped_pixels,
    compute_image_cell_box, decode_image_bytes, rgba_frame_to_dynamic_image,
};
pub use probe::{
    DEFAULT_CELL_ASPECT_RATIO, DEFAULT_CELL_PIXELS, DEFAULT_PROBE_DEADLINE,
    ESC_TIMEOUT_KITTY_KBD_MS, ESC_TIMEOUT_LOCAL_MS, ESC_TIMEOUT_SSH_MS, GeometrySource,
    ProbeOptions, ProbeReport, ProbeResponse, ProbeResponseParser, TerminalEnv,
    compute_esc_timeout_ms, negotiate_capabilities, parse_geometry_override, probe_terminal,
    probe_terminal_async, query_stdout_winsize, resolve_pixel_metrics,
};
#[cfg(unix)]
pub use probe::{probe_fd_pair, query_winsize_fd};
pub use tty_guard::{
    ResumeNotification, TerminalSignal, TtyGuard, TtyGuardConfig, build_enter_sequence,
    build_mode_refresh_sequence, build_restore_sequence, install_panic_hook, is_backspace_key,
    kitty_delete_image_data_seq, kitty_delete_image_data_seq_for, looks_like_unbracketed_paste,
    normalize_key_event, restore_terminal,
};
#[cfg(unix)]
pub use tty_guard::spawn_signal_listener;
