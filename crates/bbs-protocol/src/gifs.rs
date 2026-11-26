//! GIF catalog adapters implementing [`bbs_core::GifProvider`]:
//! - [`DiscordGifProvider`]: primary authenticated Discord `/api/v10/gifs/search` & `/gifs/trending`.
//! - [`KlipyGifProvider`]: optional Klipy HTTP adapter using a RAM-only [`SecretToken`] API key.
//! - [`GiphyGifProvider`]: optional Giphy HTTP adapter using a RAM-only [`SecretToken`] API key.
//! - Direct `.gif` / `.webp` URL resolution (`resolve_direct_gif_url` & `probe_direct_gif_url`).
//! - [`CompositeGifProvider`]: fallback chain with automatic direct-URL detection.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bbs_core::{
    CoreError, CoreResult, GifItem, GifProvider, GifProviderKind, GifSearchPage, GifSearchQuery,
    SecretToken,
};
use http::{HeaderValue, StatusCode, header};
use serde::Deserialize;
use url::Url;

use crate::fingerprint::DiscordFingerprint;
use crate::rest::DISCORD_API_BASE_URL;
use crate::wire::{deserialize_null_default, deserialize_opt_string_or_number};

/// Production Klipy API base URL.
pub const KLIPY_API_BASE_URL: &str = "https://api.klipy.com/api/v1";
/// Production Giphy API base URL.
pub const GIPHY_API_BASE_URL: &str = "https://api.giphy.com/v1/gifs";

/// Inspect whether `raw` is a direct `http(s)` URL pointing to a `.gif` or `.webp` asset.
#[must_use]
pub fn is_direct_gif_or_webp_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let path_lower = url.path().to_ascii_lowercase();
    path_lower.ends_with(".gif") || path_lower.ends_with(".webp")
}

/// Resolve a direct `.gif` or `.webp` URL into a [`GifItem`] offline (no network round-trip).
pub fn resolve_direct_gif_url(raw: &str) -> CoreResult<GifItem> {
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed)
        .map_err(|err| CoreError::Media(format!("invalid direct GIF URL: {err}")))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::Media(
            "direct GIF URL must use http or https scheme".to_owned(),
        ));
    }

    let path_lower = parsed.path().to_ascii_lowercase();
    if !path_lower.ends_with(".gif") && !path_lower.ends_with(".webp") {
        return Err(CoreError::Media(
            "direct media URL must end with .gif or .webp".to_owned(),
        ));
    }

    let filename = parsed
        .path_segments()
        .and_then(|mut segs| segs.rfind(|s| !s.is_empty()))
        .unwrap_or("direct.gif");
    let title = filename
        .strip_suffix(".gif")
        .or_else(|| filename.strip_suffix(".GIF"))
        .or_else(|| filename.strip_suffix(".webp"))
        .or_else(|| filename.strip_suffix(".WEBP"))
        .unwrap_or(filename)
        .replace(['-', '_'], " ");

    let normalized = parsed.to_string();
    Ok(GifItem {
        id: format!("direct:{filename}"),
        provider: GifProviderKind::DiscordProxy,
        title: if title.trim().is_empty() {
            "Direct GIF".to_owned()
        } else {
            title
        },
        preview_url: normalized.clone(),
        gif_url: normalized,
        width: 0,
        height: 0,
        frame_count: None,
        duration_ms: None,
        preview_rgba_frames: None,
    })
}

/// Probe a URL via HTTP `HEAD` (using a plain browser `wreq::Client`) to resolve direct `.gif`/`.webp`
/// URLs even when the path lacks a file extension (validating `Content-Type: image/gif` or `image/webp`).
pub async fn probe_direct_gif_url(client: &wreq::Client, raw: &str) -> CoreResult<GifItem> {
    if is_direct_gif_or_webp_url(raw) {
        return resolve_direct_gif_url(raw);
    }

    let parsed = Url::parse(raw.trim())
        .map_err(|err| CoreError::Media(format!("invalid direct GIF URL: {err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::Media(
            "direct GIF URL must use http or https scheme".to_owned(),
        ));
    }

    let resp = client
        .head(parsed.as_str())
        .send()
        .await
        .map_err(|_| CoreError::Network("direct GIF HEAD probe failed".to_owned()))?;

    if !resp.status().is_success() {
        return Err(CoreError::Media(format!(
            "direct GIF probe failed with HTTP {}",
            resp.status().as_u16()
        )));
    }

    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    if !content_type.starts_with("image/gif") && !content_type.starts_with("image/webp") {
        return Err(CoreError::Media(format!(
            "URL is not a GIF or WebP image (content-type: {content_type})"
        )));
    }

    let normalized = parsed.to_string();
    Ok(GifItem {
        id: format!("direct:{}", parsed.path()),
        provider: GifProviderKind::DiscordProxy,
        title: "Direct Animated Media".to_owned(),
        preview_url: normalized.clone(),
        gif_url: normalized,
        width: 0,
        height: 0,
        frame_count: None,
        duration_ms: None,
        preview_rgba_frames: None,
    })
}

// ============================================================================
// Primary Provider: Authenticated Discord GIF Proxy
// ============================================================================

/// Primary authenticated Discord GIF search proxy (`/api/v10/gifs/search` & `/api/v10/gifs/trending`).
#[derive(Clone)]
pub struct DiscordGifProvider {
    client: wreq::Client,
    token: SecretToken,
    api_base_url: String,
    media_format: String,
    upstream_provider: String,
}

impl fmt::Debug for DiscordGifProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordGifProvider")
            .field("api_base_url", &self.api_base_url)
            .field("media_format", &self.media_format)
            .field("upstream_provider", &self.upstream_provider)
            .field("token", &self.token)
            .finish()
    }
}

impl DiscordGifProvider {
    /// Create a Discord GIF provider backed by a Chrome-fingerprinted `wreq::Client`.
    pub fn new(token: SecretToken, fingerprint: &DiscordFingerprint) -> CoreResult<Self> {
        let client = fingerprint.build_rest_client()?;
        Ok(Self {
            client,
            token,
            api_base_url: DISCORD_API_BASE_URL.to_owned(),
            media_format: "webm".to_owned(),
            upstream_provider: "tenor".to_owned(),
        })
    }

    /// Override the Discord API base URL (for offline wire tests).
    #[must_use]
    pub fn with_api_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.api_base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    fn auth_header(&self) -> CoreResult<HeaderValue> {
        let mut val = HeaderValue::from_str(self.token.expose_secret())
            .map_err(|_| CoreError::AuthRequired)?;
        val.set_sensitive(true);
        Ok(val)
    }
}

#[derive(Debug, Deserialize)]
struct WireDiscordGifItem {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    src: Option<String>,
    #[serde(default)]
    gif_src: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Debug, Deserialize)]
struct WireDiscordGifEnvelope {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    results: Vec<WireDiscordGifItem>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    gifs: Vec<WireDiscordGifItem>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    next: Option<String>,
}

/// Parse Discord `/gifs/search` or `/gifs/trending` JSON payloads (supports both top-level arrays
/// and `{ results/gifs, next }` envelope shapes).
pub fn parse_discord_gifs_response(body: &[u8]) -> CoreResult<GifSearchPage> {
    if let Ok(items) = serde_json::from_slice::<Vec<WireDiscordGifItem>>(body) {
        return Ok(GifSearchPage {
            items: items
                .into_iter()
                .filter_map(map_discord_gif_item)
                .collect(),
            next_cursor: None,
        });
    }

    let envelope: WireDiscordGifEnvelope = serde_json::from_slice(body)
        .map_err(|err| CoreError::Protocol(format!("parse Discord GIF response: {err}")))?;
    let raw_items = if !envelope.results.is_empty() {
        envelope.results
    } else {
        envelope.gifs
    };

    Ok(GifSearchPage {
        items: raw_items
            .into_iter()
            .filter_map(map_discord_gif_item)
            .collect(),
        next_cursor: envelope.next.filter(|s| !s.is_empty()),
    })
}

fn map_discord_gif_item(raw: WireDiscordGifItem) -> Option<GifItem> {
    let gif_url = raw
        .gif_src
        .filter(|s| !s.is_empty())
        .or_else(|| raw.url.clone().filter(|s| !s.is_empty()))
        .or_else(|| raw.src.clone().filter(|s| !s.is_empty()))?;
    let preview_url = raw
        .preview
        .filter(|s| !s.is_empty())
        .or_else(|| raw.src.filter(|s| !s.is_empty()))
        .unwrap_or_else(|| gif_url.clone());
    let id = raw
        .id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| gif_url.clone());
    let title = raw
        .title
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "GIF".to_owned());

    Some(GifItem {
        id,
        provider: GifProviderKind::DiscordProxy,
        title,
        preview_url,
        gif_url,
        width: raw.width,
        height: raw.height,
        frame_count: None,
        duration_ms: None,
        preview_rgba_frames: None,
    })
}

#[async_trait]
impl GifProvider for DiscordGifProvider {
    fn provider_kind(&self) -> GifProviderKind {
        GifProviderKind::DiscordProxy
    }

    async fn search(&self, query: &GifSearchQuery) -> CoreResult<GifSearchPage> {
        if is_direct_gif_or_webp_url(&query.query) {
            let item = resolve_direct_gif_url(&query.query)?;
            return Ok(GifSearchPage {
                items: vec![item],
                next_cursor: None,
            });
        }
        if query.trending && query.query.trim().is_empty() {
            return self.trending(query.limit, query.cursor.as_deref()).await;
        }

        let mut url = Url::parse(&format!("{}/gifs/search", self.api_base_url))
            .map_err(|err| CoreError::Protocol(format!("build Discord GIF search URL: {err}")))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("q", query.query.trim());
            qp.append_pair("media_format", &self.media_format);
            qp.append_pair("provider", &self.upstream_provider);
            qp.append_pair(
                "locale",
                query.locale.as_deref().unwrap_or("en-US"),
            );
            qp.append_pair("limit", &query.limit.clamp(1, 50).to_string());
            if let Some(cursor) = query.cursor.as_deref().filter(|s| !s.is_empty()) {
                qp.append_pair("pos", cursor);
            }
        }

        let resp = self
            .client
            .get(url.as_str())
            .header(header::AUTHORIZATION, self.auth_header()?)
            .send()
            .await
            .map_err(|err| CoreError::Network(format!("Discord GIF search failed: {err}")))?;

        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|err| CoreError::Network(format!("read Discord GIF response: {err}")))?;

        if status == StatusCode::UNAUTHORIZED {
            return Err(CoreError::AuthRequired);
        }
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "Discord GIF search failed with HTTP {}",
                status.as_u16()
            )));
        }

        let mut page = parse_discord_gifs_response(&bytes)?;
        page.items.truncate(usize::from(query.limit.max(1)));
        Ok(page)
    }

    async fn trending(&self, limit: u8, cursor: Option<&str>) -> CoreResult<GifSearchPage> {
        let mut url = Url::parse(&format!("{}/gifs/trending", self.api_base_url))
            .map_err(|err| CoreError::Protocol(format!("build Discord GIF trending URL: {err}")))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("media_format", &self.media_format);
            qp.append_pair("provider", &self.upstream_provider);
            qp.append_pair("locale", "en-US");
            qp.append_pair("limit", &limit.clamp(1, 50).to_string());
            if let Some(pos) = cursor.filter(|s| !s.is_empty()) {
                qp.append_pair("pos", pos);
            }
        }

        let resp = self
            .client
            .get(url.as_str())
            .header(header::AUTHORIZATION, self.auth_header()?)
            .send()
            .await
            .map_err(|err| CoreError::Network(format!("Discord GIF trending failed: {err}")))?;

        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|err| CoreError::Network(format!("read Discord GIF trending body: {err}")))?;

        if status == StatusCode::UNAUTHORIZED {
            return Err(CoreError::AuthRequired);
        }
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "Discord GIF trending failed with HTTP {}",
                status.as_u16()
            )));
        }

        let mut page = parse_discord_gifs_response(&bytes)?;
        page.items.truncate(usize::from(limit.max(1)));
        Ok(page)
    }
}

// ============================================================================
// Optional Adapter: Klipy GIF Provider (RAM-only SecretToken API key)
// ============================================================================

/// Optional Klipy GIF API adapter authenticated with an in-memory [`SecretToken`] API key.
#[derive(Clone)]
pub struct KlipyGifProvider {
    client: wreq::Client,
    api_key: SecretToken,
    base_url: String,
    customer_id: String,
}

impl fmt::Debug for KlipyGifProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KlipyGifProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .finish()
    }
}

impl KlipyGifProvider {
    /// Construct a Klipy GIF provider using a RAM-only [`SecretToken`] API key.
    pub fn new(api_key: SecretToken, fingerprint: &DiscordFingerprint) -> CoreResult<Self> {
        let client = fingerprint.build_plain_browser_client()?;
        Ok(Self {
            client,
            api_key,
            base_url: KLIPY_API_BASE_URL.to_owned(),
            customer_id: "bbscord-client".to_owned(),
        })
    }

    /// Override the Klipy base URL (intended for offline wire tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }
}

#[derive(Debug, Deserialize)]
struct WireKlipyMediaVariant {
    #[serde(default)]
    url: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Debug, Deserialize)]
struct WireKlipyTier {
    #[serde(default)]
    gif: Option<WireKlipyMediaVariant>,
    #[serde(default)]
    webp: Option<WireKlipyMediaVariant>,
    #[serde(default)]
    jpg: Option<WireKlipyMediaVariant>,
}

#[derive(Debug, Deserialize)]
struct WireKlipyFile {
    #[serde(default)]
    hd: Option<WireKlipyTier>,
    #[serde(default)]
    md: Option<WireKlipyTier>,
    #[serde(default)]
    sm: Option<WireKlipyTier>,
    #[serde(default)]
    xs: Option<WireKlipyTier>,
}

#[derive(Debug, Deserialize)]
struct WireKlipyItem {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    id: Option<String>,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    file: Option<WireKlipyFile>,
}

#[derive(Debug, Deserialize)]
struct WireKlipyDataPage {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    data: Vec<WireKlipyItem>,
    #[serde(default)]
    current_page: Option<u32>,
    #[serde(default)]
    has_next: bool,
}

#[derive(Debug, Deserialize)]
struct WireKlipyResponse {
    #[serde(default)]
    data: Option<WireKlipyDataPage>,
}

/// Parse a Klipy `/gifs/search` or `/gifs/trending` JSON response into a [`GifSearchPage`].
pub fn parse_klipy_response(body: &[u8]) -> CoreResult<GifSearchPage> {
    let resp: WireKlipyResponse = serde_json::from_slice(body)
        .map_err(|err| CoreError::Protocol(format!("parse Klipy GIF response: {err}")))?;
    let Some(page) = resp.data else {
        return Ok(GifSearchPage::default());
    };

    let items = page
        .data
        .into_iter()
        .filter_map(|raw| {
            let file = raw.file?;
            let full = file
                .hd
                .as_ref()
                .and_then(|t| t.gif.as_ref().or(t.webp.as_ref()))
                .or_else(|| {
                    file.md
                        .as_ref()
                        .and_then(|t| t.gif.as_ref().or(t.webp.as_ref()))
                })
                .or_else(|| {
                    file.sm
                        .as_ref()
                        .and_then(|t| t.gif.as_ref().or(t.webp.as_ref()))
                })?;
            if full.url.is_empty() {
                return None;
            }
            let preview = file
                .sm
                .as_ref()
                .and_then(|t| t.webp.as_ref().or(t.gif.as_ref()).or(t.jpg.as_ref()))
                .or_else(|| {
                    file.xs
                        .as_ref()
                        .and_then(|t| t.webp.as_ref().or(t.gif.as_ref()).or(t.jpg.as_ref()))
                })
                .unwrap_or(full);

            let id = raw
                .id
                .or(raw.slug)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| full.url.clone());
            let title = raw
                .title
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Klipy GIF".to_owned());

            Some(GifItem {
                id,
                provider: GifProviderKind::Klipy,
                title,
                preview_url: if preview.url.is_empty() {
                    full.url.clone()
                } else {
                    preview.url.clone()
                },
                gif_url: full.url.clone(),
                width: full.width,
                height: full.height,
                frame_count: None,
                duration_ms: None,
                preview_rgba_frames: None,
            })
        })
        .collect();

    let next_cursor = if page.has_next {
        Some(page.current_page.unwrap_or(1).saturating_add(1).to_string())
    } else {
        None
    };

    Ok(GifSearchPage { items, next_cursor })
}

#[async_trait]
impl GifProvider for KlipyGifProvider {
    fn provider_kind(&self) -> GifProviderKind {
        GifProviderKind::Klipy
    }

    async fn search(&self, query: &GifSearchQuery) -> CoreResult<GifSearchPage> {
        if is_direct_gif_or_webp_url(&query.query) {
            return Ok(GifSearchPage {
                items: vec![resolve_direct_gif_url(&query.query)?],
                next_cursor: None,
            });
        }
        if query.trending && query.query.trim().is_empty() {
            return self.trending(query.limit, query.cursor.as_deref()).await;
        }

        let page_num = query
            .cursor
            .as_deref()
            .and_then(|c| c.parse::<u32>().ok())
            .unwrap_or(1);

        // Sanitize all errors so `api_key` in the URL path can NEVER appear in logs/errors.
        let mut url = Url::parse(&format!(
            "{}/{}/gifs/search",
            self.base_url,
            self.api_key.expose_secret()
        ))
        .map_err(|_| CoreError::Protocol("failed to build Klipy search URL".to_owned()))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("q", query.query.trim());
            qp.append_pair("page", &page_num.to_string());
            qp.append_pair("per_page", &query.limit.clamp(1, 50).to_string());
            qp.append_pair("customer_id", &self.customer_id);
            if let Some(locale) = query.locale.as_deref() {
                qp.append_pair("locale", locale);
            }
        }

        let resp = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(|_| CoreError::Network("Klipy GIF search network error".to_owned()))?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(CoreError::AuthRequired);
        }
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "Klipy GIF search failed with HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|_| CoreError::Network("failed reading Klipy GIF search body".to_owned()))?;
        parse_klipy_response(&bytes)
    }

    async fn trending(&self, limit: u8, cursor: Option<&str>) -> CoreResult<GifSearchPage> {
        let page_num = cursor.and_then(|c| c.parse::<u32>().ok()).unwrap_or(1);
        let mut url = Url::parse(&format!(
            "{}/{}/gifs/trending",
            self.base_url,
            self.api_key.expose_secret()
        ))
        .map_err(|_| CoreError::Protocol("failed to build Klipy trending URL".to_owned()))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("page", &page_num.to_string());
            qp.append_pair("per_page", &limit.clamp(1, 50).to_string());
            qp.append_pair("customer_id", &self.customer_id);
        }

        let resp = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(|_| CoreError::Network("Klipy GIF trending network error".to_owned()))?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(CoreError::AuthRequired);
        }
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "Klipy GIF trending failed with HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|_| CoreError::Network("failed reading Klipy GIF trending body".to_owned()))?;
        parse_klipy_response(&bytes)
    }
}

// ============================================================================
// Optional Adapter: Giphy GIF Provider (RAM-only SecretToken API key)
// ============================================================================

/// Optional Giphy HTTP adapter authenticated with an in-memory [`SecretToken`] API key.
#[derive(Clone)]
pub struct GiphyGifProvider {
    client: wreq::Client,
    api_key: SecretToken,
    base_url: String,
    rating: String,
}

impl fmt::Debug for GiphyGifProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GiphyGifProvider")
            .field("base_url", &self.base_url)
            .field("rating", &self.rating)
            .field("api_key", &self.api_key)
            .finish()
    }
}

impl GiphyGifProvider {
    /// Construct a Giphy GIF provider using a RAM-only [`SecretToken`] API key.
    pub fn new(api_key: SecretToken, fingerprint: &DiscordFingerprint) -> CoreResult<Self> {
        let client = fingerprint.build_plain_browser_client()?;
        Ok(Self {
            client,
            api_key,
            base_url: GIPHY_API_BASE_URL.to_owned(),
            rating: "pg-13".to_owned(),
        })
    }

    /// Override the Giphy base URL (intended for offline wire tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }
}

#[derive(Debug, Deserialize)]
struct WireGiphyRendition {
    #[serde(default)]
    url: String,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    width: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    height: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_number")]
    frames: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireGiphyImages {
    #[serde(default)]
    original: Option<WireGiphyRendition>,
    #[serde(default)]
    fixed_width_small: Option<WireGiphyRendition>,
    #[serde(default)]
    preview_gif: Option<WireGiphyRendition>,
    #[serde(default)]
    downsized: Option<WireGiphyRendition>,
}

#[derive(Debug, Deserialize)]
struct WireGiphyItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    images: Option<WireGiphyImages>,
}

#[derive(Debug, Deserialize)]
struct WireGiphyPagination {
    #[serde(default)]
    total_count: u32,
    #[serde(default)]
    count: u32,
    #[serde(default)]
    offset: u32,
}

#[derive(Debug, Deserialize)]
struct WireGiphyResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    data: Vec<WireGiphyItem>,
    #[serde(default)]
    pagination: Option<WireGiphyPagination>,
}

/// Parse a Giphy `/v1/gifs/search` or `/v1/gifs/trending` JSON payload into a [`GifSearchPage`].
pub fn parse_giphy_response(body: &[u8]) -> CoreResult<GifSearchPage> {
    let resp: WireGiphyResponse = serde_json::from_slice(body)
        .map_err(|err| CoreError::Protocol(format!("parse Giphy GIF response: {err}")))?;

    let items = resp
        .data
        .into_iter()
        .filter_map(|raw| {
            let images = raw.images?;
            let orig = images.original.as_ref().or(images.downsized.as_ref())?;
            if orig.url.is_empty() {
                return None;
            }
            let preview = images
                .fixed_width_small
                .as_ref()
                .or(images.preview_gif.as_ref())
                .or(images.downsized.as_ref())
                .unwrap_or(orig);

            let width = orig
                .width
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let height = orig
                .height
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            let frame_count = orig
                .frames
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok());

            Some(GifItem {
                id: if raw.id.is_empty() {
                    orig.url.clone()
                } else {
                    raw.id
                },
                provider: GifProviderKind::Giphy,
                title: if raw.title.trim().is_empty() {
                    "Giphy GIF".to_owned()
                } else {
                    raw.title
                },
                preview_url: if preview.url.is_empty() {
                    orig.url.clone()
                } else {
                    preview.url.clone()
                },
                gif_url: orig.url.clone(),
                width,
                height,
                frame_count,
                duration_ms: None,
                preview_rgba_frames: None,
            })
        })
        .collect();

    let next_cursor = resp.pagination.and_then(|p| {
        let next_offset = p.offset.saturating_add(p.count);
        if p.count > 0 && next_offset < p.total_count {
            Some(next_offset.to_string())
        } else {
            None
        }
    });

    Ok(GifSearchPage { items, next_cursor })
}

#[async_trait]
impl GifProvider for GiphyGifProvider {
    fn provider_kind(&self) -> GifProviderKind {
        GifProviderKind::Giphy
    }

    async fn search(&self, query: &GifSearchQuery) -> CoreResult<GifSearchPage> {
        if is_direct_gif_or_webp_url(&query.query) {
            return Ok(GifSearchPage {
                items: vec![resolve_direct_gif_url(&query.query)?],
                next_cursor: None,
            });
        }
        if query.trending && query.query.trim().is_empty() {
            return self.trending(query.limit, query.cursor.as_deref()).await;
        }

        let offset = query
            .cursor
            .as_deref()
            .and_then(|c| c.parse::<u32>().ok())
            .unwrap_or(0);

        let mut url = Url::parse(&format!("{}/search", self.base_url))
            .map_err(|_| CoreError::Protocol("failed to build Giphy search URL".to_owned()))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("api_key", self.api_key.expose_secret());
            qp.append_pair("q", query.query.trim());
            qp.append_pair("limit", &query.limit.clamp(1, 50).to_string());
            qp.append_pair("offset", &offset.to_string());
            qp.append_pair("rating", &self.rating);
            if let Some(locale) = query.locale.as_deref()
                && let Some(lang) = locale.split(['-', '_']).next()
            {
                qp.append_pair("lang", lang);
            }
        }

        // Sanitize errors so `api_key` query param can never leak.
        let resp = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(|_| CoreError::Network("Giphy GIF search network error".to_owned()))?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(CoreError::AuthRequired);
        }
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "Giphy GIF search failed with HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|_| CoreError::Network("failed reading Giphy GIF search body".to_owned()))?;
        parse_giphy_response(&bytes)
    }

    async fn trending(&self, limit: u8, cursor: Option<&str>) -> CoreResult<GifSearchPage> {
        let offset = cursor.and_then(|c| c.parse::<u32>().ok()).unwrap_or(0);
        let mut url = Url::parse(&format!("{}/trending", self.base_url))
            .map_err(|_| CoreError::Protocol("failed to build Giphy trending URL".to_owned()))?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("api_key", self.api_key.expose_secret());
            qp.append_pair("limit", &limit.clamp(1, 50).to_string());
            qp.append_pair("offset", &offset.to_string());
            qp.append_pair("rating", &self.rating);
        }

        let resp = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(|_| CoreError::Network("Giphy GIF trending network error".to_owned()))?;

        let status = resp.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(CoreError::AuthRequired);
        }
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "Giphy GIF trending failed with HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|_| CoreError::Network("failed reading Giphy GIF trending body".to_owned()))?;
        parse_giphy_response(&bytes)
    }
}

// ============================================================================
// Composite Fallback Chain (Primary Discord Proxy -> Optional Klipy / Giphy)
// ============================================================================

/// Multi-provider GIF resolver: resolves direct `.gif`/`.webp` URLs immediately, otherwise
/// queries the primary provider (Discord proxy) and falls back to optional configured adapters.
pub struct CompositeGifProvider {
    primary: Arc<dyn GifProvider>,
    fallbacks: Vec<Arc<dyn GifProvider>>,
}

impl fmt::Debug for CompositeGifProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fallback_kinds: Vec<GifProviderKind> =
            self.fallbacks.iter().map(|p| p.provider_kind()).collect();
        f.debug_struct("CompositeGifProvider")
            .field("primary", &self.primary.provider_kind())
            .field("fallbacks", &fallback_kinds)
            .finish()
    }
}

impl CompositeGifProvider {
    /// Construct a composite provider with a primary backend and ordered optional fallbacks.
    #[must_use]
    pub fn new(primary: Arc<dyn GifProvider>, fallbacks: Vec<Arc<dyn GifProvider>>) -> Self {
        Self { primary, fallbacks }
    }
}

#[async_trait]
impl GifProvider for CompositeGifProvider {
    fn provider_kind(&self) -> GifProviderKind {
        self.primary.provider_kind()
    }

    async fn search(&self, query: &GifSearchQuery) -> CoreResult<GifSearchPage> {
        if is_direct_gif_or_webp_url(&query.query) {
            return Ok(GifSearchPage {
                items: vec![resolve_direct_gif_url(&query.query)?],
                next_cursor: None,
            });
        }

        let mut last_err: Option<CoreError> = None;
        match self.primary.search(query).await {
            Ok(page) if !page.items.is_empty() || self.fallbacks.is_empty() => return Ok(page),
            Ok(_) => {}
            Err(err) => last_err = Some(err),
        }

        for fallback in &self.fallbacks {
            match fallback.search(query).await {
                Ok(page) if !page.items.is_empty() => return Ok(page),
                Ok(_) => {}
                Err(err) => last_err = Some(err),
            }
        }

        match last_err {
            Some(err) => Err(err),
            None => Ok(GifSearchPage::default()),
        }
    }

    async fn trending(&self, limit: u8, cursor: Option<&str>) -> CoreResult<GifSearchPage> {
        let mut last_err: Option<CoreError> = None;
        match self.primary.trending(limit, cursor).await {
            Ok(page) if !page.items.is_empty() || self.fallbacks.is_empty() => return Ok(page),
            Ok(_) => {}
            Err(err) => last_err = Some(err),
        }

        for fallback in &self.fallbacks {
            match fallback.trending(limit, cursor).await {
                Ok(page) if !page.items.is_empty() => return Ok(page),
                Ok(_) => {}
                Err(err) => last_err = Some(err),
            }
        }

        match last_err {
            Some(err) => Err(err),
            None => Ok(GifSearchPage::default()),
        }
    }
}
