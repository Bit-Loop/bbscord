//! `bbs-protocol`: Discord user-account TLS/HTTP2 fingerprinting, REST API v10, Group DMs,
//! DM/Group DM call signaling, QR Remote-Auth v2, Gateway v10 (`zlib-stream`), and GIF search.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod fingerprint;
pub mod gateway;
pub mod gifs;
pub mod remote_auth;
pub mod rest;
pub mod wire;

pub use bbs_core;
pub use fingerprint::{
    DEFAULT_BROWSER_MAJOR_VERSION, DEFAULT_BROWSER_NAME, DEFAULT_CLIENT_BUILD_NUMBER,
    DiscordFingerprint, FingerprintConfig, IdentifyProperties, LAUNCH_SIGNATURE_MASK,
    SuperProperties, apply_launch_signature_mask, detect_system_timezone,
    generate_launch_signature, sec_ch_ua_brands,
};
pub use gateway::{
    DISCORD_GATEWAY_URL, DiscordGateway, GatewayCommand, GatewayConfig, GatewayControl,
    GatewayFrameOutcome, GatewayHandle, GatewaySessionState, GatewaySignalingEvent,
    GuildSubscriptionPayload, ReadStateEntry, RelationshipEntry, ZLIB_SYNC_FLUSH_SUFFIX,
    ZlibStreamInflater, capabilities, opcodes, parse_stream_key,
};
pub use gifs::{
    CompositeGifProvider, DiscordGifProvider, GIPHY_API_BASE_URL, GiphyGifProvider,
    KLIPY_API_BASE_URL, KlipyGifProvider, is_direct_gif_or_webp_url, probe_direct_gif_url,
    resolve_direct_gif_url,
};
pub use remote_auth::{
    REMOTE_AUTH_GATEWAY_URL, REMOTE_AUTH_QR_PREFIX, RemoteAuthClient, RemoteAuthEvent,
    RemoteAuthKeypair, RemoteAuthPendingUser, RemoteAuthServerFrame,
};
pub use rest::{
    DISCORD_API_BASE_URL, DiscordRestClient, RateLimitConfig, RateLimiter, TokenValidationCheck,
    UploadAttachment, encode_reaction_emoji, is_discord_cdn_url, preserve_cdn_resize_params,
    validate_discord_token,
};
