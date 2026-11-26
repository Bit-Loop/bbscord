//! Unified error types for `bbs-core` and downstream crates.

use thiserror::Error;

/// Core error type shared across protocol, media, voice, video, and TUI contracts.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// Failed to parse a Discord Snowflake ID.
    #[error("invalid snowflake: {0}")]
    InvalidSnowflake(String),

    /// Authentication token is missing, rejected, or expired.
    #[error("authentication required")]
    AuthRequired,

    /// Caller lacks permission for the requested Discord operation.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Requested guild, channel, message, user, or stream does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Operation was rate-limited by Discord or an upstream API.
    #[error("rate limited; retry after {retry_after_ms}ms")]
    RateLimited {
        /// Suggested retry backoff in milliseconds.
        retry_after_ms: u64,
    },

    /// Transport or connectivity error.
    #[error("network error: {0}")]
    Network(String),

    /// Protocol serialization, handshake, or state machine error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Voice engine, Opus, audio device, or DAVE E2EE error.
    #[error("voice error: {0}")]
    Voice(String),

    /// Video / Go Live screenshare capture or decode error.
    #[error("video error: {0}")]
    Video(String),

    /// Image, GIF, or attachment processing error.
    #[error("media error: {0}")]
    Media(String),

    /// Keyring or secret management error (always sanitized; never includes secret bytes).
    #[error("secret store error: {0}")]
    SecretStore(String),

    /// Theme syntax or color parsing error.
    #[error("theme parse error: {0}")]
    ThemeParse(String),

    /// Requested capability or action is not supported in the active backend/environment.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// Operation was cancelled before completion.
    #[error("operation cancelled")]
    Cancelled,

    /// Internal invariant violation or unexpected state.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience result alias using [`CoreError`].
pub type CoreResult<T> = Result<T, CoreError>;
