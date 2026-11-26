//! Error types for `bbs-voice`.

use bbs_core::error::CoreError;
use thiserror::Error;

/// Unified error type for voice signaling, UDP/RTP transport, DAVE E2EE, Opus, and audio I/O.
#[derive(Debug, Error)]
pub enum VoiceError {
    /// Attempted an operation that requires an active voice session.
    #[error("not connected to a voice channel or call")]
    NotConnected,

    /// Invalid or unsupported voice channel / call target.
    #[error("invalid voice target: {0}")]
    InvalidTarget(String),

    /// Main gateway signaling (`VOICE_STATE_UPDATE` / `VOICE_SERVER_UPDATE` / `CALL_*`) failed.
    #[error("voice signaling failed: {0}")]
    Signaling(String),

    /// Voice Gateway v8 WebSocket protocol error.
    #[error("voice gateway error: {0}")]
    Gateway(String),

    /// UDP IP discovery handshake failed or returned a malformed 74-byte response.
    #[error("UDP IP discovery error: {0}")]
    Discovery(String),

    /// RTP packet parsing or framing error.
    #[error("RTP error: {0}")]
    Rtp(String),

    /// Discord UDP transport AEAD (`aead_xchacha20_poly1305_rtpsize` / `aead_aes256_gcm_rtpsize`) error.
    #[error("transport AEAD error: {0}")]
    TransportCrypto(String),

    /// DAVE MLS E2EE handshake, verification, or frame encryption/decryption error.
    #[error("DAVE E2EE error: {0}")]
    Dave(String),

    /// Opus codec initialization, encoding, or decoding error.
    #[error("Opus codec error: {0}")]
    Opus(String),

    /// Audio hardware or `cpal` stream error.
    #[error("audio device error: {0}")]
    AudioDevice(String),

    /// Ogg/Opus voice message container parsing or playback error.
    #[error("Ogg/Opus voice message error: {0}")]
    Ogg(String),

    /// Underlying OS I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Internal lock or state invariant error.
    #[error("internal voice error: {0}")]
    Internal(String),
}

/// Convenience result alias for `bbs-voice`.
pub type VoiceResult<T> = Result<T, VoiceError>;

impl From<VoiceError> for CoreError {
    fn from(err: VoiceError) -> Self {
        match err {
            VoiceError::InvalidTarget(msg) => Self::Protocol(msg),
            VoiceError::Io(err) => Self::Network(err.to_string()),
            other => Self::Voice(other.to_string()),
        }
    }
}
