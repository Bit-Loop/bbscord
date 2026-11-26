//! `bbs-core`: shared domain models, viewport/theme contracts, zeroized secrets,
//! async service traits, unified event bus, and offline deterministic mocks for `bbscord`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod events;
pub mod mock;
pub mod model;
pub mod secrets;
pub mod theme;
pub mod traits;
pub mod viewport;

/// Workspace version exported for diagnostics and CLI `--version`.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use error::{CoreError, CoreResult};
pub use events::{BackendEvent, ConnectionState, UiAction};
pub use mock::{
    MockDiscordBackend, MockGifProvider, MockVideoStreamEngine, MockVoiceEngine, MockVoiceRoster,
    mock_ids,
};
pub use model::{
    Activity, ActivityKind, Attachment, CallRingState, Channel, ChannelKind, DaveSessionState,
    Embed, EmbedAuthor, EmbedField, EmbedFooter, EmbedKind, EmbedMedia, EmbedProvider, GifItem,
    GifProviderKind, GifSearchPage, GifSearchQuery, Guild, GuildFolder, GuildMemberSummary,
    Message, MessagePatch, MessageReference, PollAnswerSummary, PollSummary, Presence,
    PresenceStatus, Reaction, RefreshedAttachmentUrl, RgbaFrame, Role, Snowflake, StreamDescriptor,
    StreamQuality, StreamSourceKind, User, VideoFrame, VideoSinkKind, VoiceChannelStatus,
    VoiceConnectionState, VoiceMessageMeta, VoiceParticipant, VoicePlaybackState,
    resize_discord_media_proxy_url,
};
pub use secrets::{
    BBSCORD_KEYRING_SERVICE, BBSCORD_KEYRING_USER, DISCORDO_KEYRING_SERVICE,
    DISCORDO_KEYRING_USER, KeyringStore, MemoryKeyringStore, OsKeyringStore, SecretToken,
    TokenOrigin,
};
pub use theme::{
    Ansi16Gradient, Ansi16Palette, ColorDepth, ColorGradient, RgbColor, ThemePalette, ThemeSource,
    ThemeSpec, parse_btop_theme,
};
pub use traits::{DiscordBackend, GifProvider, VideoStreamEngine, VoiceEngine};
pub use viewport::{
    AnchorPart, CharsetMode, GraphicsTier, KeyboardProtocol, LayoutProfile, MultiplexerKind,
    ResolvedCharset, ScrollAnchor, ScrollState, TerminalCapabilities, ViewportGeometry,
};
