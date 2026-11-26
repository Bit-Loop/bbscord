//! Discord API v10 REST backend (`DiscordBackend`), typed rate-limit bucket manager,
//! Group DM CRUD, DM/Group DM call signaling, multipart file/GIF uploads, CDN URL refresh,
//! and token validation.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bbs_core::{
    BackendEvent, CallRingState, Channel, CoreError, CoreResult, DiscordBackend, Guild,
    GuildFolder, Message, RefreshedAttachmentUrl, SecretToken, Snowflake, User,
};
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, warn};
use wreq::multipart::{Form, Part};

use crate::fingerprint::DiscordFingerprint;
pub use crate::remote_auth::{
    REMOTE_AUTH_GATEWAY_URL, REMOTE_AUTH_QR_PREFIX, RemoteAuthClient, RemoteAuthEvent,
    RemoteAuthKeypair, RemoteAuthPendingUser,
};
use crate::wire::{
    WireApiError, WireChannel, WireGuild, WireGuildFolder, WireMessage, WireRefreshUrlsResponse,
    WireUser,
};

/// Production Discord REST API v10 base URL.
pub const DISCORD_API_BASE_URL: &str = "https://discord.com/api/v10";

/// Percent-encoding set for reaction emoji path segments (encodes non-URI-safe bytes and `/` `?` `#` `%` space).
const REACTION_EMOJI_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Percent-encode a Unicode glyph (`"🔥"`) or custom emoji identifier (`"rust:12345"`) for Discord reaction URLs.
#[must_use]
pub fn encode_reaction_emoji(emoji: &str) -> String {
    let trimmed = emoji.trim().trim_matches(':');
    utf8_percent_encode(trimmed, REACTION_EMOJI_ENCODE_SET).to_string()
}

/// Binary file or GIF payload prepared for multipart upload on `POST /channels/{id}/messages`.
#[derive(Debug, Clone)]
pub struct UploadAttachment {
    /// Filename transmitted in multipart headers and Discord attachment metadata.
    pub filename: String,
    /// Optional MIME type (e.g. `"image/gif"`, `"image/png"`, `"audio/ogg"`).
    pub content_type: Option<String>,
    /// Optional accessibility alt-text / description.
    pub description: Option<String>,
    /// Raw file bytes.
    pub bytes: Vec<u8>,
}

/// Retry and backoff policy for HTTP `429 Too Many Requests` responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Maximum automatic retry attempts after receiving HTTP 429.
    pub max_retries: u32,
    /// Upper bound on automatic sleep duration; longer waits return [`CoreError::RateLimited`].
    pub max_retry_wait: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            max_retry_wait: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone)]
struct BucketState {
    remaining: u32,
    reset_at: Instant,
}

#[derive(Debug, Default)]
struct RateLimitState {
    route_to_bucket: HashMap<String, String>,
    buckets: HashMap<String, BucketState>,
    global_reset_at: Option<Instant>,
}

/// Typed per-route and per-bucket Discord REST rate-limit tracker.
#[derive(Debug, Default)]
pub struct RateLimiter {
    state: Mutex<RateLimitState>,
}

impl RateLimiter {
    /// Create a fresh empty rate-limit tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Compute how long to wait before dispatching a request on `route_key`.
    pub async fn pending_wait(&self, route_key: &str) -> Option<Duration> {
        let now = Instant::now();
        let guard = self.state.lock().await;
        let mut wait: Option<Duration> = None;

        if let Some(global_reset) = guard.global_reset_at
            && global_reset > now
        {
            wait = Some(global_reset.saturating_duration_since(now));
        }

        if let Some(bucket_id) = guard.route_to_bucket.get(route_key)
            && let Some(bucket) = guard.buckets.get(bucket_id)
            && bucket.remaining == 0
            && bucket.reset_at > now
        {
            let bucket_wait = bucket.reset_at.saturating_duration_since(now);
            wait = Some(wait.map_or(bucket_wait, |w| w.max(bucket_wait)));
        }

        wait
    }

    /// Sleep until any active global or bucket rate-limit window on `route_key` has elapsed,
    /// returning [`CoreError::RateLimited`] if the required wait exceeds `max_wait`.
    pub async fn acquire(&self, route_key: &str, max_wait: Duration) -> CoreResult<()> {
        if let Some(wait) = self.pending_wait(route_key).await {
            if wait > max_wait {
                return Err(CoreError::RateLimited {
                    retry_after_ms: duration_to_ms_u64(wait),
                });
            }
            tokio::time::sleep(wait).await;
        }
        Ok(())
    }

    /// Update tracked bucket limits from Discord `X-RateLimit-*` response headers.
    pub async fn observe_headers(&self, route_key: &str, headers: &HeaderMap) {
        let bucket_id = header_str(headers, "x-ratelimit-bucket").map(str::to_owned);
        let remaining = header_str(headers, "x-ratelimit-remaining")
            .and_then(|s| s.parse::<u32>().ok());
        let reset_after = header_str(headers, "x-ratelimit-reset-after")
            .and_then(parse_seconds_duration);
        let is_global = header_str(headers, "x-ratelimit-global")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));

        let now = Instant::now();
        let mut guard = self.state.lock().await;

        if is_global
            && let Some(retry) = header_str(headers, "retry-after")
                .and_then(parse_seconds_duration)
                .or(reset_after)
        {
            guard.global_reset_at = Some(now + retry);
        }

        if let Some(bucket_hash) = bucket_id {
            let scoped_bucket = scoped_bucket_key(route_key, &bucket_hash);
            guard
                .route_to_bucket
                .insert(route_key.to_owned(), scoped_bucket.clone());
            if let (Some(rem), Some(reset_dur)) = (remaining, reset_after) {
                guard.buckets.insert(
                    scoped_bucket,
                    BucketState {
                        remaining: rem,
                        reset_at: now + reset_dur,
                    },
                );
            }
        }
    }

    /// Record a `429 Too Many Requests` backoff window on the route's bucket or globally.
    pub async fn record_429(&self, route_key: &str, retry_after: Duration, is_global: bool) {
        let reset_at = Instant::now() + retry_after;
        let mut guard = self.state.lock().await;
        if is_global {
            guard.global_reset_at = Some(reset_at);
            return;
        }
        let bucket_id = guard
            .route_to_bucket
            .entry(route_key.to_owned())
            .or_insert_with(|| format!("route:{route_key}"))
            .clone();
        guard.buckets.insert(
            bucket_id,
            BucketState {
                remaining: 0,
                reset_at,
            },
        );
    }
}

/// Scope a Discord `X-RateLimit-Bucket` hash by the route's major parameter (`channel_id` or
/// `guild_id`) so two different channels sharing a Discord route bucket hash never block each other.
fn scoped_bucket_key(route_key: &str, bucket_hash: &str) -> String {
    let major = route_key
        .split_whitespace()
        .nth(1)
        .and_then(|path| {
            let mut segs = path.trim_start_matches('/').split('/');
            match (segs.next(), segs.next()) {
                (Some("channels"), Some(id)) => Some(format!("channel:{id}")),
                (Some("guilds"), Some(id)) => Some(format!("guild:{id}")),
                _ => None,
            }
        })
        .unwrap_or_else(|| "global".to_owned());
    format!("{major}:{bucket_hash}")
}

/// Passive status result from validating a Discord user token (contains zero PII or secrets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenValidationCheck {
    /// HTTP status code returned by `GET /api/v10/users/@me`.
    pub status_code: u16,
    /// Whether the response body matched the expected Discord User JSON schema (`id` + `username`).
    pub schema_valid: bool,
}

/// Production Discord REST v10 backend powered by Chrome-fingerprinted `wreq 0.15`.
#[derive(Clone)]
pub struct DiscordRestClient {
    client: wreq::Client,
    cdn_client: wreq::Client,
    fingerprint: DiscordFingerprint,
    token: SecretToken,
    api_base_url: String,
    rate_limiter: Arc<RateLimiter>,
    rate_limit_config: RateLimitConfig,
    events_tx: broadcast::Sender<BackendEvent>,
    guild_folders_cache: Arc<RwLock<Vec<GuildFolder>>>,
}

impl fmt::Debug for DiscordRestClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordRestClient")
            .field("api_base_url", &self.api_base_url)
            .field("token", &self.token)
            .field("rate_limit_config", &self.rate_limit_config)
            .finish()
    }
}

impl DiscordRestClient {
    /// Construct a new Discord REST client using the provided zeroizing token and Chrome fingerprint.
    pub fn new(token: SecretToken, fingerprint: DiscordFingerprint) -> CoreResult<Self> {
        let client = fingerprint.build_rest_client()?;
        let cdn_client = fingerprint.build_plain_browser_client()?;
        let (events_tx, _) = broadcast::channel(512);
        Ok(Self {
            client,
            cdn_client,
            fingerprint,
            token,
            api_base_url: DISCORD_API_BASE_URL.to_owned(),
            rate_limiter: Arc::new(RateLimiter::new()),
            rate_limit_config: RateLimitConfig::default(),
            events_tx,
            guild_folders_cache: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Override the Discord API base URL (e.g. `http://127.0.0.1:<port>/api/v10` for offline tests).
    #[must_use]
    pub fn with_api_base_url(mut self, api_base_url: impl Into<String>) -> Self {
        self.api_base_url = api_base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Override the rate-limit retry policy.
    #[must_use]
    pub const fn with_rate_limit_config(mut self, config: RateLimitConfig) -> Self {
        self.rate_limit_config = config;
        self
    }

    /// Attach a shared [`broadcast::Sender<BackendEvent>`] (e.g. shared with [`crate::gateway::DiscordGateway`]).
    #[must_use]
    pub fn with_event_sender(mut self, events_tx: broadcast::Sender<BackendEvent>) -> Self {
        self.events_tx = events_tx;
        self
    }

    /// Return the Chrome session fingerprint backing this client.
    #[must_use]
    pub const fn fingerprint(&self) -> &DiscordFingerprint {
        &self.fingerprint
    }

    /// Return the configured API base URL.
    #[must_use]
    pub fn api_base_url(&self) -> &str {
        &self.api_base_url
    }

    /// Return the underlying fingerprinted `wreq::Client`.
    #[must_use]
    pub const fn http_client(&self) -> &wreq::Client {
        &self.client
    }

    /// Return a clone of the backend event broadcast sender.
    #[must_use]
    pub fn event_sender(&self) -> broadcast::Sender<BackendEvent> {
        self.events_tx.clone()
    }

    /// Update cached sidebar guild folders (typically populated by Gateway `READY`).
    pub fn set_cached_guild_folders(&self, folders: Vec<GuildFolder>) {
        if let Ok(mut guard) = self.guild_folders_cache.write() {
            *guard = folders;
        }
    }

    /// Validate the configured token via a single passive `GET /users/@me` request,
    /// returning the parsed [`User`] without logging any credentials or PII.
    pub async fn validate_token(&self) -> CoreResult<User> {
        self.current_user().await
    }

    /// Perform a single passive `GET /users/@me` schema verification returning ONLY HTTP status
    /// and boolean schema validity (no user ID, username, email, or token ever exposed).
    pub async fn check_token_contract_readonly(&self) -> CoreResult<TokenValidationCheck> {
        let resp = self
            .execute_raw(Method::GET, "/users/@me", "GET /users/@me", RequestBody::Empty)
            .await?;
        let status_code = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|err| CoreError::Network(format!("read /users/@me body: {err}")))?;

        if status_code != StatusCode::OK.as_u16() {
            return Ok(TokenValidationCheck {
                status_code,
                schema_valid: false,
            });
        }

        let schema_valid = serde_json::from_slice::<WireUser>(&body)
            .is_ok_and(|u| u.id.as_u64() > 0 && !u.username.trim().is_empty());

        Ok(TokenValidationCheck {
            status_code,
            schema_valid,
        })
    }

    /// Send a message with optional local file or GIF attachments via Discord v10 multipart upload.
    pub async fn send_message_with_attachments(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
        attachments: &[UploadAttachment],
    ) -> CoreResult<Message> {
        if attachments.is_empty() {
            return self.send_message(channel_id, content, reply_to).await;
        }

        let nonce = generate_message_nonce();
        let attachment_meta: Vec<MultipartAttachmentMeta<'_>> = attachments
            .iter()
            .enumerate()
            .map(|(idx, att)| MultipartAttachmentMeta {
                id: idx,
                filename: &att.filename,
                description: att.description.as_deref(),
            })
            .collect();

        let payload = CreateMessagePayload {
            content,
            nonce: Some(&nonce),
            tts: false,
            flags: 0,
            message_reference: reply_to.map(|message_id| OutboundMessageReference {
                message_id,
                fail_if_not_exists: false,
            }),
            attachments: Some(&attachment_meta),
        };

        let payload_json = serde_json::to_string(&payload)
            .map_err(|err| CoreError::Protocol(format!("serialize multipart payload_json: {err}")))?;

        let path = format!("/channels/{channel_id}/messages");
        let route_key = format!("POST /channels/{channel_id}/messages");

        let wire: WireMessage = self
            .execute_json(
                Method::POST,
                &path,
                &route_key,
                RequestBody::Multipart {
                    payload_json,
                    attachments: attachments.to_vec(),
                },
            )
            .await?;

        let msg = Message::from(wire);
        let _ = self.events_tx.send(BackendEvent::MessageCreated(msg.clone()));
        Ok(msg)
    }

    /// Download attachment bytes from a signed Discord CDN URL, automatically refreshing expired
    /// URLs via `POST /attachments/refresh-urls` on any non-200 HTTP response (matching
    /// `discordo/internal/ui/chat/messageslist/attachments.go:170-196`).
    pub async fn fetch_attachment_bytes(
        &self,
        url: &str,
    ) -> CoreResult<(Vec<u8>, Option<RefreshedAttachmentUrl>)> {
        let initial_resp = self
            .cdn_client
            .get(url)
            .send()
            .await
            .map_err(|err| CoreError::Network(format!("CDN attachment GET failed: {err}")))?;

        if initial_resp.status() == StatusCode::OK {
            let bytes = initial_resp
                .bytes()
                .await
                .map_err(|err| CoreError::Network(format!("read CDN attachment body: {err}")))?;
            return Ok((bytes.to_vec(), None));
        }

        debug!(
            status = initial_resp.status().as_u16(),
            "CDN attachment fetch returned non-200; refreshing signed URL"
        );
        drop(initial_resp);

        let refreshed = self.refresh_attachment_url(url).await?;
        let retry_resp = self
            .cdn_client
            .get(&refreshed.refreshed_url)
            .send()
            .await
            .map_err(|err| CoreError::Network(format!("refreshed CDN GET failed: {err}")))?;

        if retry_resp.status() != StatusCode::OK {
            return Err(CoreError::Media(format!(
                "CDN attachment fetch failed after refresh with HTTP {}",
                retry_resp.status().as_u16()
            )));
        }

        let bytes = retry_resp
            .bytes()
            .await
            .map_err(|err| CoreError::Network(format!("read refreshed CDN body: {err}")))?;
        Ok((bytes.to_vec(), Some(refreshed)))
    }

    /// Download attachment bytes and emit [`BackendEvent::AttachmentUrlRefreshed`] if a CDN refresh occurred.
    pub async fn fetch_message_attachment_bytes(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        attachment_id: Snowflake,
        url: &str,
    ) -> CoreResult<Vec<u8>> {
        let (bytes, refreshed) = self.fetch_attachment_bytes(url).await?;
        if let Some(info) = refreshed {
            let _ = self.events_tx.send(BackendEvent::AttachmentUrlRefreshed {
                channel_id,
                message_id,
                attachment_id,
                url: info.refreshed_url,
                expires_at_unix: info.expires_at_unix,
            });
        }
        Ok(bytes)
    }

    /// Fetch a single channel object via `GET /channels/{channel_id}`.
    pub async fn get_channel(&self, channel_id: Snowflake) -> CoreResult<Channel> {
        let path = format!("/channels/{channel_id}");
        let route_key = format!("GET /channels/{channel_id}");
        let wire: WireChannel = self
            .execute_json(Method::GET, &path, &route_key, RequestBody::Empty)
            .await?;
        Ok(wire.into_core_channel(None, &HashMap::new()))
    }

    fn auth_header_value(&self) -> CoreResult<HeaderValue> {
        let mut val = HeaderValue::from_str(self.token.expose_secret())
            .map_err(|_| CoreError::AuthRequired)?;
        val.set_sensitive(true);
        Ok(val)
    }

    async fn execute_raw(
        &self,
        method: Method,
        path: &str,
        route_key: &str,
        body: RequestBody,
    ) -> CoreResult<wreq::Response> {
        let url = format!("{}{}", self.api_base_url, path);
        let auth_header = self.auth_header_value()?;

        for attempt in 0..=self.rate_limit_config.max_retries {
            self.rate_limiter
                .acquire(route_key, self.rate_limit_config.max_retry_wait)
                .await?;

            let mut req = self
                .client
                .request(method.clone(), &url)
                .header(header::AUTHORIZATION, auth_header.clone());

            match &body {
                RequestBody::Empty => {}
                RequestBody::Json(json_bytes) => {
                    req = req
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(json_bytes.clone());
                }
                RequestBody::Multipart {
                    payload_json,
                    attachments,
                } => {
                    let form = build_multipart_form(payload_json, attachments)?;
                    req = req.multipart(form);
                }
            }

            let resp = req
                .send()
                .await
                .map_err(|err| CoreError::Network(format!("HTTP {method} {route_key}: {err}")))?;

            self.rate_limiter
                .observe_headers(route_key, resp.headers())
                .await;

            if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                let header_retry = header_str(resp.headers(), "retry-after")
                    .and_then(parse_seconds_duration);
                let header_global = header_str(resp.headers(), "x-ratelimit-global")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"));
                let err_body = resp.bytes().await.unwrap_or_default();
                let parsed_err = serde_json::from_slice::<WireApiError>(&err_body).ok();

                let body_retry = parsed_err
                    .as_ref()
                    .and_then(|e| e.retry_after)
                    .and_then(secs_f64_to_duration);
                let is_global = header_global
                    || parsed_err
                        .as_ref()
                        .and_then(|e| e.global)
                        .unwrap_or(false);
                let retry_after = body_retry
                    .or(header_retry)
                    .unwrap_or_else(|| Duration::from_millis(1_000));

                self.rate_limiter
                    .record_429(route_key, retry_after, is_global)
                    .await;

                let retry_after_ms = duration_to_ms_u64(retry_after);
                if attempt >= self.rate_limit_config.max_retries
                    || retry_after > self.rate_limit_config.max_retry_wait
                {
                    return Err(CoreError::RateLimited { retry_after_ms });
                }

                warn!(
                    route = route_key,
                    attempt = attempt + 1,
                    retry_after_ms,
                    global = is_global,
                    "Discord REST 429 rate-limited; backing off before retry"
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }

            return Ok(resp);
        }

        Err(CoreError::RateLimited {
            retry_after_ms: 1_000,
        })
    }

    async fn execute_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        route_key: &str,
        body: RequestBody,
    ) -> CoreResult<T> {
        let resp = self.execute_raw(method, path, route_key, body).await?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|err| CoreError::Network(format!("read response body ({route_key}): {err}")))?;

        if !status.is_success() {
            return Err(map_http_error(status, &bytes, route_key));
        }

        serde_json::from_slice(&bytes).map_err(|err| {
            CoreError::Protocol(format!("decode JSON response for {route_key}: {err}"))
        })
    }

    async fn execute_no_content(
        &self,
        method: Method,
        path: &str,
        route_key: &str,
        body: RequestBody,
    ) -> CoreResult<()> {
        let resp = self.execute_raw(method, path, route_key, body).await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = resp.bytes().await.unwrap_or_default();
        Err(map_http_error(status, &bytes, route_key))
    }
}

#[derive(Debug, Clone)]
enum RequestBody {
    Empty,
    Json(Vec<u8>),
    Multipart {
        payload_json: String,
        attachments: Vec<UploadAttachment>,
    },
}

impl RequestBody {
    fn json<T: Serialize>(value: &T) -> CoreResult<Self> {
        let bytes = serde_json::to_vec(value)
            .map_err(|err| CoreError::Protocol(format!("serialize REST JSON body: {err}")))?;
        Ok(Self::Json(bytes))
    }
}

fn build_multipart_form(
    payload_json: &str,
    attachments: &[UploadAttachment],
) -> CoreResult<Form> {
    let payload_part = Part::text(payload_json.to_owned())
        .mime_str("application/json")
        .map_err(|err| CoreError::Protocol(format!("multipart payload_json mime: {err}")))?;
    let mut form = Form::new().part("payload_json", payload_part);

    for (idx, att) in attachments.iter().enumerate() {
        let mut part = Part::bytes(att.bytes.clone()).file_name(att.filename.clone());
        if let Some(ct) = att.content_type.as_deref() {
            part = part
                .mime_str(ct)
                .map_err(|err| CoreError::Protocol(format!("invalid attachment MIME {ct}: {err}")))?;
        }
        form = form.part(format!("files[{idx}]"), part);
    }

    Ok(form)
}

#[derive(Debug, Serialize)]
struct OutboundMessageReference {
    message_id: Snowflake,
    fail_if_not_exists: bool,
}

#[derive(Debug, Serialize)]
struct MultipartAttachmentMeta<'a> {
    id: usize,
    filename: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct CreateMessagePayload<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<&'a str>,
    tts: bool,
    flags: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<OutboundMessageReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<&'a [MultipartAttachmentMeta<'a>]>,
}

#[derive(Debug, Serialize)]
struct EditMessagePayload<'a> {
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct CreateGroupDmPayload<'a> {
    recipients: &'a [Snowflake],
}

#[derive(Debug, Serialize)]
struct RenameChannelPayload<'a> {
    name: &'a str,
}

#[derive(Debug, Serialize)]
struct CallRingPayload<'a> {
    recipients: Option<&'a [Snowflake]>,
}

#[derive(Debug, Serialize)]
struct RefreshUrlsRequest<'a> {
    attachment_urls: &'a [&'a str],
}

#[derive(Debug, Deserialize)]
struct WireUserSettings {
    #[serde(default)]
    guild_folders: Vec<WireGuildFolder>,
}

#[async_trait]
impl DiscordBackend for DiscordRestClient {
    async fn current_user(&self) -> CoreResult<User> {
        let wire: WireUser = self
            .execute_json(Method::GET, "/users/@me", "GET /users/@me", RequestBody::Empty)
            .await?;
        Ok(User::from(wire))
    }

    async fn list_guilds(&self) -> CoreResult<(Vec<Guild>, Vec<GuildFolder>)> {
        let wire_guilds: Vec<WireGuild> = self
            .execute_json(
                Method::GET,
                "/users/@me/guilds",
                "GET /users/@me/guilds",
                RequestBody::Empty,
            )
            .await?;
        let guilds: Vec<Guild> = wire_guilds.iter().map(WireGuild::to_core_guild).collect();

        let cached_folders = self
            .guild_folders_cache
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();

        if !cached_folders.is_empty() {
            return Ok((guilds, cached_folders));
        }

        // Best-effort fetch of legacy JSON user settings folders if Gateway READY has not seeded cache yet.
        let folders = match self
            .execute_json::<WireUserSettings>(
                Method::GET,
                "/users/@me/settings",
                "GET /users/@me/settings",
                RequestBody::Empty,
            )
            .await
        {
            Ok(settings) => {
                let mapped: Vec<GuildFolder> = settings
                    .guild_folders
                    .into_iter()
                    .map(GuildFolder::from)
                    .collect();
                self.set_cached_guild_folders(mapped.clone());
                mapped
            }
            Err(_) => Vec::new(),
        };

        Ok((guilds, folders))
    }

    async fn list_channels(&self, guild_id: Snowflake) -> CoreResult<Vec<Channel>> {
        let path = format!("/guilds/{guild_id}/channels");
        let route_key = format!("GET /guilds/{guild_id}/channels");
        let wire: Vec<WireChannel> = self
            .execute_json(Method::GET, &path, &route_key, RequestBody::Empty)
            .await?;
        let empty_users = HashMap::new();
        let mut channels: Vec<Channel> = wire
            .into_iter()
            .map(|c| c.into_core_channel(Some(guild_id), &empty_users))
            .collect();
        channels.sort_by_key(|c| (c.position, c.id));
        Ok(channels)
    }

    async fn list_dm_channels(&self) -> CoreResult<Vec<Channel>> {
        let wire: Vec<WireChannel> = self
            .execute_json(
                Method::GET,
                "/users/@me/channels",
                "GET /users/@me/channels",
                RequestBody::Empty,
            )
            .await?;
        let empty_users = HashMap::new();
        let mut channels: Vec<Channel> = wire
            .into_iter()
            .map(|c| c.into_core_channel(None, &empty_users))
            .collect();
        // Order DMs & Group DMs by most recent message descending (Discord sidebar convention).
        channels.sort_by(|a, b| b.last_message_id.cmp(&a.last_message_id).then(b.id.cmp(&a.id)));
        Ok(channels)
    }

    async fn fetch_messages(
        &self,
        channel_id: Snowflake,
        before: Option<Snowflake>,
        limit: u8,
    ) -> CoreResult<Vec<Message>> {
        let clamped_limit = limit.clamp(1, 100);
        let path = match before {
            Some(before_id) => {
                format!("/channels/{channel_id}/messages?limit={clamped_limit}&before={before_id}")
            }
            None => format!("/channels/{channel_id}/messages?limit={clamped_limit}"),
        };
        let route_key = format!("GET /channels/{channel_id}/messages");
        let wire: Vec<WireMessage> = self
            .execute_json(Method::GET, &path, &route_key, RequestBody::Empty)
            .await?;

        // Discord returns messages newest-first; reverse to chronological (oldest -> newest).
        let mut messages: Vec<Message> = wire.into_iter().map(Message::from).collect();
        messages.reverse();
        Ok(messages)
    }

    async fn send_message(
        &self,
        channel_id: Snowflake,
        content: &str,
        reply_to: Option<Snowflake>,
    ) -> CoreResult<Message> {
        let nonce = generate_message_nonce();
        let payload = CreateMessagePayload {
            content,
            nonce: Some(&nonce),
            tts: false,
            flags: 0,
            message_reference: reply_to.map(|message_id| OutboundMessageReference {
                message_id,
                fail_if_not_exists: false,
            }),
            attachments: None,
        };
        let path = format!("/channels/{channel_id}/messages");
        let route_key = format!("POST /channels/{channel_id}/messages");
        let wire: WireMessage = self
            .execute_json(Method::POST, &path, &route_key, RequestBody::json(&payload)?)
            .await?;
        let msg = Message::from(wire);
        let _ = self.events_tx.send(BackendEvent::MessageCreated(msg.clone()));
        Ok(msg)
    }

    async fn edit_message(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        content: &str,
    ) -> CoreResult<Message> {
        let payload = EditMessagePayload { content };
        let path = format!("/channels/{channel_id}/messages/{message_id}");
        let route_key = format!("PATCH /channels/{channel_id}/messages");
        let wire: WireMessage = self
            .execute_json(Method::PATCH, &path, &route_key, RequestBody::json(&payload)?)
            .await?;
        let msg = Message::from(wire);
        let _ = self.events_tx.send(BackendEvent::MessageUpdated(msg.clone()));
        Ok(msg)
    }

    async fn delete_message(&self, channel_id: Snowflake, message_id: Snowflake) -> CoreResult<()> {
        let path = format!("/channels/{channel_id}/messages/{message_id}");
        let route_key = format!("DELETE /channels/{channel_id}/messages");
        self.execute_no_content(Method::DELETE, &path, &route_key, RequestBody::Empty)
            .await?;
        let _ = self.events_tx.send(BackendEvent::MessageDeleted {
            channel_id,
            message_id,
        });
        Ok(())
    }

    async fn add_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> CoreResult<()> {
        let encoded = encode_reaction_emoji(emoji);
        let path = format!("/channels/{channel_id}/messages/{message_id}/reactions/{encoded}/@me");
        let route_key = format!("PUT /channels/{channel_id}/messages/reactions");
        self.execute_no_content(Method::PUT, &path, &route_key, RequestBody::Empty)
            .await
    }

    async fn remove_reaction(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        emoji: &str,
    ) -> CoreResult<()> {
        let encoded = encode_reaction_emoji(emoji);
        let path = format!("/channels/{channel_id}/messages/{message_id}/reactions/{encoded}/@me");
        let route_key = format!("DELETE /channels/{channel_id}/messages/reactions");
        self.execute_no_content(Method::DELETE, &path, &route_key, RequestBody::Empty)
            .await
    }

    async fn send_typing(&self, channel_id: Snowflake) -> CoreResult<()> {
        let path = format!("/channels/{channel_id}/typing");
        let route_key = format!("POST /channels/{channel_id}/typing");
        self.execute_no_content(Method::POST, &path, &route_key, RequestBody::Empty)
            .await
    }

    async fn create_group_dm(
        &self,
        recipient_ids: &[Snowflake],
        name: Option<&str>,
    ) -> CoreResult<Channel> {
        if recipient_ids.is_empty() {
            return Err(CoreError::Protocol(
                "create_group_dm requires at least one recipient".to_owned(),
            ));
        }

        let payload = CreateGroupDmPayload {
            recipients: recipient_ids,
        };
        let wire: WireChannel = self
            .execute_json(
                Method::POST,
                "/users/@me/channels",
                "POST /users/@me/channels",
                RequestBody::json(&payload)?,
            )
            .await?;
        let mut channel = wire.into_core_channel(None, &HashMap::new());

        if let Some(desired_name) = name.map(str::trim).filter(|s| !s.is_empty()) {
            channel = self.rename_group_dm(channel.id, desired_name).await?;
        } else {
            let _ = self
                .events_tx
                .send(BackendEvent::ChannelUpserted(channel.clone()));
        }

        Ok(channel)
    }

    async fn add_group_dm_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> CoreResult<Channel> {
        let path = format!("/channels/{channel_id}/recipients/{user_id}");
        let route_key = format!("PUT /channels/{channel_id}/recipients");
        let resp = self
            .execute_raw(Method::PUT, &path, &route_key, RequestBody::Empty)
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(map_http_error(status, &bytes, &route_key));
        }

        let channel = if !bytes.is_empty()
            && let Ok(wire) = serde_json::from_slice::<WireChannel>(&bytes)
        {
            wire.into_core_channel(None, &HashMap::new())
        } else {
            self.get_channel(channel_id).await?
        };

        let _ = self
            .events_tx
            .send(BackendEvent::ChannelUpserted(channel.clone()));
        Ok(channel)
    }

    async fn remove_group_dm_recipient(
        &self,
        channel_id: Snowflake,
        user_id: Snowflake,
    ) -> CoreResult<Channel> {
        let path = format!("/channels/{channel_id}/recipients/{user_id}");
        let route_key = format!("DELETE /channels/{channel_id}/recipients");
        let resp = self
            .execute_raw(Method::DELETE, &path, &route_key, RequestBody::Empty)
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(map_http_error(status, &bytes, &route_key));
        }

        let channel = if !bytes.is_empty()
            && let Ok(wire) = serde_json::from_slice::<WireChannel>(&bytes)
        {
            wire.into_core_channel(None, &HashMap::new())
        } else {
            self.get_channel(channel_id).await?
        };

        let _ = self
            .events_tx
            .send(BackendEvent::ChannelUpserted(channel.clone()));
        Ok(channel)
    }

    async fn rename_group_dm(&self, channel_id: Snowflake, name: &str) -> CoreResult<Channel> {
        let payload = RenameChannelPayload { name };
        let path = format!("/channels/{channel_id}");
        let route_key = format!("PATCH /channels/{channel_id}");
        let wire: WireChannel = self
            .execute_json(
                Method::PATCH,
                &path,
                &route_key,
                RequestBody::json(&payload)?,
            )
            .await?;
        let channel = wire.into_core_channel(None, &HashMap::new());
        let _ = self
            .events_tx
            .send(BackendEvent::ChannelUpserted(channel.clone()));
        Ok(channel)
    }

    async fn leave_group_dm(&self, channel_id: Snowflake) -> CoreResult<()> {
        let path = format!("/channels/{channel_id}?silent=false");
        let route_key = format!("DELETE /channels/{channel_id}");
        self.execute_no_content(Method::DELETE, &path, &route_key, RequestBody::Empty)
            .await?;
        let _ = self.events_tx.send(BackendEvent::ChannelDeleted {
            guild_id: None,
            channel_id,
        });
        Ok(())
    }

    async fn ring_call(
        &self,
        channel_id: Snowflake,
        recipient_ids: Option<&[Snowflake]>,
    ) -> CoreResult<CallRingState> {
        let payload = CallRingPayload {
            recipients: recipient_ids,
        };
        let path = format!("/channels/{channel_id}/call/ring");
        let route_key = format!("POST /channels/{channel_id}/call/ring");
        self.execute_no_content(Method::POST, &path, &route_key, RequestBody::json(&payload)?)
            .await?;

        let state = CallRingState::OutgoingRinging {
            channel_id,
            target_user_ids: recipient_ids.map(<[Snowflake]>::to_vec).unwrap_or_default(),
        };
        let _ = self.events_tx.send(BackendEvent::CallRingUpdated(state.clone()));
        Ok(state)
    }

    async fn stop_call_ring(&self, channel_id: Snowflake) -> CoreResult<CallRingState> {
        let payload = CallRingPayload { recipients: None };
        let path = format!("/channels/{channel_id}/call/stop-ringing");
        let route_key = format!("POST /channels/{channel_id}/call/stop-ringing");
        self.execute_no_content(Method::POST, &path, &route_key, RequestBody::json(&payload)?)
            .await?;

        let _ = self
            .events_tx
            .send(BackendEvent::CallRingUpdated(CallRingState::Idle));
        Ok(CallRingState::Idle)
    }

    async fn refresh_attachment_url(
        &self,
        original_url: &str,
    ) -> CoreResult<RefreshedAttachmentUrl> {
        let urls = [original_url];
        let payload = RefreshUrlsRequest {
            attachment_urls: &urls,
        };
        let resp: WireRefreshUrlsResponse = self
            .execute_json(
                Method::POST,
                "/attachments/refresh-urls",
                "POST /attachments/refresh-urls",
                RequestBody::json(&payload)?,
            )
            .await?;

        let mut entry = resp.refreshed_urls.into_iter().next().ok_or_else(|| {
            CoreError::Media("Discord attachments/refresh-urls returned empty list".to_owned())
        })?;
        if entry.original.is_empty() {
            entry.original = original_url.to_owned();
        }
        Ok(RefreshedAttachmentUrl::from(entry))
    }

    fn subscribe_events(&self) -> broadcast::Receiver<BackendEvent> {
        self.events_tx.subscribe()
    }
}

/// Standalone helper to validate a Discord user token with a fresh Chrome fingerprint.
pub async fn validate_discord_token(token: SecretToken) -> CoreResult<User> {
    let fp = DiscordFingerprint::default_chrome()?;
    let client = DiscordRestClient::new(token, fp)?;
    client.validate_token().await
}

fn generate_message_nonce() -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let discord_ms = now_ms.saturating_sub(Snowflake::DISCORD_EPOCH_MS);
    let rand_low: u64 = rand::random::<u32>() as u64 & 0x003F_FFFF;
    let nonce_snowflake = (discord_ms << 22) | rand_low;
    nonce_snowflake.to_string()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

fn parse_seconds_duration(raw: &str) -> Option<Duration> {
    let secs: f64 = raw.trim().parse().ok()?;
    secs_f64_to_duration(secs)
}

fn secs_f64_to_duration(secs: f64) -> Option<Duration> {
    if !secs.is_finite() || secs.is_sign_negative() {
        return None;
    }
    let clamped = secs.min(3_600.0);
    Some(Duration::from_secs_f64(clamped))
}

fn duration_to_ms_u64(dur: Duration) -> u64 {
    u64::try_from(dur.as_millis()).unwrap_or(u64::MAX)
}

fn map_http_error(status: StatusCode, body: &[u8], route_key: &str) -> CoreError {
    let parsed = serde_json::from_slice::<WireApiError>(body).ok();
    let detail = match parsed {
        Some(WireApiError {
            code: Some(code),
            message: Some(msg),
            ..
        }) => format!("{route_key}: HTTP {} (code {code}: {msg})", status.as_u16()),
        Some(WireApiError {
            message: Some(msg),
            ..
        }) => format!("{route_key}: HTTP {} ({msg})", status.as_u16()),
        _ => format!("{route_key}: HTTP {}", status.as_u16()),
    };

    match status {
        StatusCode::UNAUTHORIZED => CoreError::AuthRequired,
        StatusCode::FORBIDDEN => CoreError::PermissionDenied(detail),
        StatusCode::NOT_FOUND => CoreError::NotFound(detail),
        StatusCode::TOO_MANY_REQUESTS => CoreError::RateLimited {
            retry_after_ms: 1_000,
        },
        s if s.is_server_error() => CoreError::Network(detail),
        _ => CoreError::Protocol(detail),
    }
}
