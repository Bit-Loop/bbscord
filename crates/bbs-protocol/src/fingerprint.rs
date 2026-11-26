//! Chrome TLS/HTTP2 fingerprinting, Chromium GREASE `Sec-Ch-Ua` derivation,
//! `X-Super-Properties` / `IdentifyProperties`, and masked `launch_signature` UUIDv4.
//!
//! Cross-referenced against `discordo/internal/{http,tls}` so wire headers, TLS extensions,
//! HTTP/2 framing, and Discord client properties match real desktop Chrome behavior.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bbs_core::{CoreError, CoreResult};
use http::{HeaderMap, HeaderName, HeaderValue, header};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wreq::cookie::Jar;
use wreq::{
    AlpnProtos, AlpsProtos, CertCompressionAlgorithm, Client, EmulationProvider, Http1Config,
    Http2Config, PseudoOrder, SettingsOrder, SslCurve, StreamDependency, StreamId, TlsConfig,
    TlsVersion,
};

/// Default browser product name advertised in User-Agent, `Sec-Ch-Ua`, and super properties.
pub const DEFAULT_BROWSER_NAME: &str = "Chrome";
/// Default Chrome major version matching `discordo/internal/tls/profile.go` (`Chrome_152`).
pub const DEFAULT_BROWSER_MAJOR_VERSION: u32 = 152;
/// Default OS name advertised in super properties and `Sec-Ch-Ua-Platform`.
pub const DEFAULT_OS_NAME: &str = "Windows";
/// Default OS version advertised in super properties.
pub const DEFAULT_OS_VERSION: &str = "10";
/// Default locale advertised in `X-Discord-Locale` and `system_locale`.
pub const DEFAULT_LOCALE: &str = "en-US";
/// Default Discord web client build number (synced with `discordo/internal/http/generated.go`).
pub const DEFAULT_CLIENT_BUILD_NUMBER: u64 = 649_389;
/// Canonical Discord origin.
pub const DISCORD_ORIGIN: &str = "https://discord.com";
/// Default Discord web app referer.
pub const DISCORD_REFERER: &str = "https://discord.com/channels/@me";

/// Bitmask clearing Discord client-modification detection bits from `launch_signature` UUIDv4.
///
/// Exact byte vector from `discordo/internal/http/properties.go:97-102`.
/// Preserves RFC 4122 UUID version 4 bits (byte 6 high nibble `0b0100`) and variant bits
/// (byte 8 high 2 bits `0b10`).
pub const LAUNCH_SIGNATURE_MASK: [u8; 16] = [
    0b1111_1111,
    0b0111_1111,
    0b1110_1111,
    0b1110_1111,
    0b1111_0111,
    0b1110_1111,
    0b1111_0111,
    0b1111_1111,
    0b1101_1111,
    0b0111_1110,
    0b1111_1111,
    0b1011_1111,
    0b1111_1110,
    0b1111_1111,
    0b1111_0111,
    0b1111_1111,
];

/// Modern desktop Chrome TLS curves (post-quantum X25519MLKEM768 + classical X25519/P-256/P-384).
const CHROME_CURVES: &[SslCurve] = &[
    SslCurve::X25519_MLKEM768,
    SslCurve::X25519,
    SslCurve::SECP256R1,
    SslCurve::SECP384R1,
];

/// Chrome TLS 1.3 + TLS 1.2 BoringSSL cipher list.
const CHROME_CIPHER_LIST: &str = concat!(
    "TLS_AES_128_GCM_SHA256:",
    "TLS_AES_256_GCM_SHA384:",
    "TLS_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256:",
    "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256:",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384:",
    "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256:",
    "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA:",
    "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA:",
    "TLS_RSA_WITH_AES_128_GCM_SHA256:",
    "TLS_RSA_WITH_AES_256_GCM_SHA384:",
    "TLS_RSA_WITH_AES_128_CBC_SHA:",
    "TLS_RSA_WITH_AES_256_CBC_SHA"
);

/// Chrome TLS signature algorithms list.
const CHROME_SIGALGS_LIST: &str = concat!(
    "ecdsa_secp256r1_sha256:",
    "rsa_pss_rsae_sha256:",
    "rsa_pkcs1_sha256:",
    "ecdsa_secp384r1_sha384:",
    "rsa_pss_rsae_sha384:",
    "rsa_pkcs1_sha384:",
    "rsa_pss_rsae_sha512:",
    "rsa_pkcs1_sha512"
);

/// Chrome TLS 1.3 certificate compression (Brotli only).
const CHROME_CERT_COMPRESSION: &[CertCompressionAlgorithm] = &[CertCompressionAlgorithm::Brotli];

/// Standard zoneinfo prefixes checked when resolving `/etc/localtime` symlinks on Unix.
const ZONEINFO_PREFIXES: &[&str] = &[
    "/usr/share/zoneinfo/",
    "../usr/share/zoneinfo/",
    "/etc/zoneinfo/",
    "../etc/zoneinfo/",
    "/var/db/timezone/zoneinfo/",
];

/// Chrome-like header emission order for HTTP/1.1 WebSocket upgrade requests.
static CHROME_WS_HEADER_ORDER: &[HeaderName] = &[
    header::HOST,
    header::CONNECTION,
    header::PRAGMA,
    header::CACHE_CONTROL,
    header::USER_AGENT,
    header::UPGRADE,
    header::ORIGIN,
    header::SEC_WEBSOCKET_VERSION,
    header::ACCEPT_ENCODING,
    header::ACCEPT_LANGUAGE,
    header::COOKIE,
    header::SEC_WEBSOCKET_KEY,
];

/// Chrome-like header emission order for REST requests.
static CHROME_HEADER_ORDER: &[HeaderName] = &[
    header::HOST,
    header::CONNECTION,
    header::CONTENT_LENGTH,
    header::AUTHORIZATION,
    header::USER_AGENT,
    header::CONTENT_TYPE,
    header::ACCEPT,
    header::ORIGIN,
    header::REFERER,
    header::ACCEPT_ENCODING,
    header::ACCEPT_LANGUAGE,
    header::COOKIE,
];

/// Derive the Chromium GREASE `Sec-Ch-Ua` brand list header from browser name and major version.
///
/// Matches Chromium's `GenerateBrandVersionList` / `ShuffleBrandList` and
/// `discordo/internal/http/properties.go:31-48`.
#[must_use]
pub fn sec_ch_ua_brands(browser_name: &str, major_version: u32) -> String {
    const GREASE_CHARS: [&str; 11] = [" ", "(", ":", "-", ".", "/", ")", ";", "=", "?", "_"];
    const GREASE_VERSIONS: [&str; 3] = ["8", "99", "24"];
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    let seed = major_version as usize;
    let list = [
        format!(
            "\"Not{}A{}Brand\";v=\"{}\"",
            GREASE_CHARS[seed % 11],
            GREASE_CHARS[(seed + 1) % 11],
            GREASE_VERSIONS[seed % 3]
        ),
        format!("\"Chromium\";v=\"{major_version}\""),
        format!("\"Google {browser_name}\";v=\"{major_version}\""),
    ];

    let order = ORDERS[seed % 6];
    let mut shuffled: [&str; 3] = ["", "", ""];
    for (i, &target_slot) in order.iter().enumerate() {
        shuffled[target_slot] = list[i].as_str();
    }
    shuffled.join(", ")
}

/// Apply Discord's `launch_signature` UUIDv4 client-mod detection bitmask to raw UUID bytes.
///
/// Ensures RFC 4122 version 4 and variant bits are set before applying [`LAUNCH_SIGNATURE_MASK`].
#[must_use]
pub const fn apply_launch_signature_mask(mut bytes: [u8; 16]) -> [u8; 16] {
    // Enforce RFC 4122 UUIDv4 version (0100xxxx) and variant (10xxxxxx).
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;

    let mut i = 0;
    while i < 16 {
        bytes[i] &= LAUNCH_SIGNATURE_MASK[i];
        i += 1;
    }
    bytes
}

/// Generate a fresh masked UUIDv4 suitable for Discord's `launch_signature` super property.
#[must_use]
pub fn generate_launch_signature() -> String {
    let raw = *Uuid::new_v4().as_bytes();
    let masked = apply_launch_signature_mask(raw);
    Uuid::from_bytes(masked).to_string()
}

/// Extract an IANA timezone name from a symlink target (e.g. `/usr/share/zoneinfo/America/Chicago`).
#[must_use]
pub fn zone_from_localtime_link(target: &str) -> Option<String> {
    for prefix in ZONEINFO_PREFIXES {
        if let Some(rest) = target.strip_prefix(prefix) {
            let trimmed = rest.trim().trim_start_matches('/');
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    // Also handle relative paths containing `/zoneinfo/` (e.g. `../../usr/share/zoneinfo/UTC`).
    if let Some((_, rest)) = target.split_once("/zoneinfo/") {
        let trimmed = rest.trim().trim_start_matches('/');
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// Detect the system IANA timezone name from explicit `localtime` and `timezone` file paths.
#[must_use]
pub fn detect_system_timezone_from_paths(
    localtime_path: &Path,
    timezone_path: &Path,
) -> Option<String> {
    if let Ok(link_target) = std::fs::read_link(localtime_path)
        && let Some(target_str) = link_target.to_str()
        && let Some(zone) = zone_from_localtime_link(target_str)
    {
        return Some(zone);
    }

    if let Ok(contents) = std::fs::read_to_string(timezone_path) {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }

    None
}

/// Detect the local system IANA timezone (matching `discordo/pkg/tzlocal`), falling back to `"UTC"`.
#[must_use]
pub fn detect_system_timezone() -> String {
    if let Some(zone) =
        detect_system_timezone_from_paths(Path::new("/etc/localtime"), Path::new("/etc/timezone"))
    {
        return zone;
    }

    if let Ok(tz_env) = std::env::var("TZ") {
        let cleaned = tz_env.trim().trim_start_matches(':');
        if !cleaned.is_empty() && cleaned.contains('/') {
            return cleaned.to_owned();
        }
    }

    "UTC".to_owned()
}

/// Configurable client fingerprint parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintConfig {
    /// Browser name (`"Chrome"`).
    pub browser_name: String,
    /// Browser major version (`152`).
    pub browser_major_version: u32,
    /// Operating system name (`"Windows"`).
    pub os_name: String,
    /// Operating system version (`"10"`).
    pub os_version: String,
    /// BCP-47 locale (`"en-US"`).
    pub locale: String,
    /// System IANA timezone name (`"America/Chicago"`, `"UTC"`, etc.).
    pub timezone: String,
    /// Discord desktop/web client build number.
    pub client_build_number: u64,
    /// Discord release channel (`"stable"`).
    pub release_channel: String,
    /// Request timeout for REST calls.
    pub request_timeout: Duration,
    /// TCP/TLS connect timeout.
    pub connect_timeout: Duration,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        Self {
            browser_name: DEFAULT_BROWSER_NAME.to_owned(),
            browser_major_version: DEFAULT_BROWSER_MAJOR_VERSION,
            os_name: DEFAULT_OS_NAME.to_owned(),
            os_version: DEFAULT_OS_VERSION.to_owned(),
            locale: DEFAULT_LOCALE.to_owned(),
            timezone: detect_system_timezone(),
            client_build_number: DEFAULT_CLIENT_BUILD_NUMBER,
            release_channel: "stable".to_owned(),
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(15),
        }
    }
}

impl FingerprintConfig {
    /// Format the full browser version string (`"<major>.0.0.0"`).
    #[must_use]
    pub fn browser_version(&self) -> String {
        format!("{}.0.0.0", self.browser_major_version)
    }

    /// Construct the desktop Chrome `User-Agent` string matching `discordo/internal/http/properties.go:22-24`.
    #[must_use]
    pub fn browser_user_agent(&self) -> String {
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) {}/{} Safari/537.36",
            self.browser_name,
            self.browser_version()
        )
    }

    /// Derive the Chromium GREASE `Sec-Ch-Ua` header for this config.
    #[must_use]
    pub fn sec_ch_ua(&self) -> String {
        sec_ch_ua_brands(&self.browser_name, self.browser_major_version)
    }
}

/// Gateway `IDENTIFY` `properties` object matching Discord web client & `discordo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentifyProperties {
    /// Operating system name (`"Windows"`).
    pub os: String,
    /// Browser name (`"Chrome"`).
    pub browser: String,
    /// Device name (`""` for desktop web).
    pub device: String,
    /// System locale (`"en-US"`).
    pub system_locale: String,
    /// Whether client mods are loaded (`false`).
    pub has_client_mods: bool,
    /// Browser `User-Agent` string.
    pub browser_user_agent: String,
    /// Browser full version (`"152.0.0.0"`).
    pub browser_version: String,
    /// Operating system version (`"10"`).
    pub os_version: String,
    /// Initial HTTP referrer.
    pub referrer: String,
    /// Initial referring domain.
    pub referring_domain: String,
    /// Current referrer.
    pub referrer_current: String,
    /// Current referring domain.
    pub referring_domain_current: String,
    /// Discord release channel (`"stable"`).
    pub release_channel: String,
    /// Discord web client build number.
    pub client_build_number: u64,
    /// Client event source (`null` on desktop web).
    pub client_event_source: Option<String>,
    /// Per-launch UUIDv4.
    pub client_launch_id: String,
    /// Fast-connect hint sent in Gateway `IDENTIFY`.
    pub is_fast_connect: bool,
}

/// REST `X-Super-Properties` JSON payload (Base64-encoded on the wire) matching `discordo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuperProperties {
    /// Operating system name (`"Windows"`).
    pub os: String,
    /// Browser name (`"Chrome"`).
    pub browser: String,
    /// Device name (`""`).
    pub device: String,
    /// System locale (`"en-US"`).
    pub system_locale: String,
    /// Whether client mods are loaded (`false`).
    pub has_client_mods: bool,
    /// Browser `User-Agent` string.
    pub browser_user_agent: String,
    /// Browser full version.
    pub browser_version: String,
    /// Operating system version.
    pub os_version: String,
    /// Initial HTTP referrer.
    pub referrer: String,
    /// Initial referring domain.
    pub referring_domain: String,
    /// Current referrer.
    pub referrer_current: String,
    /// Current referring domain.
    pub referring_domain_current: String,
    /// Discord release channel (`"stable"`).
    pub release_channel: String,
    /// Discord web client build number.
    pub client_build_number: u64,
    /// Client event source (`null`).
    pub client_event_source: Option<String>,
    /// Per-launch UUIDv4.
    pub client_launch_id: String,
    /// Masked UUIDv4 clearing Discord client-mod detection bits.
    pub launch_signature: String,
    /// Per-session heartbeat UUIDv4.
    pub client_heartbeat_session_id: String,
    /// Current application focus state (`"focused"`).
    pub client_app_state: String,
}

impl SuperProperties {
    /// Encode these properties as standard Base64 JSON for the `X-Super-Properties` header.
    pub fn to_base64(&self) -> CoreResult<String> {
        let json_bytes = serde_json::to_vec(self)
            .map_err(|err| CoreError::Protocol(format!("serialize X-Super-Properties: {err}")))?;
        Ok(BASE64_STANDARD.encode(json_bytes))
    }

    /// Decode a Base64 `X-Super-Properties` header back into [`SuperProperties`].
    pub fn from_base64(encoded: &str) -> CoreResult<Self> {
        let raw = BASE64_STANDARD
            .decode(encoded.trim())
            .map_err(|err| CoreError::Protocol(format!("decode X-Super-Properties base64: {err}")))?;
        serde_json::from_slice(&raw)
            .map_err(|err| CoreError::Protocol(format!("parse X-Super-Properties json: {err}")))
    }
}

/// Complete immutable session fingerprint (headers, properties, and persistent cookie jar)
/// used to construct Chrome-impersonating `wreq` REST and WebSocket clients.
#[derive(Debug, Clone)]
pub struct DiscordFingerprint {
    config: FingerprintConfig,
    user_agent: String,
    sec_ch_ua: String,
    identify_properties: IdentifyProperties,
    super_properties: SuperProperties,
    super_properties_b64: String,
    cookie_jar: Arc<Jar>,
}

impl DiscordFingerprint {
    /// Build a new session fingerprint with fresh `client_launch_id`, `client_heartbeat_session_id`,
    /// masked `launch_signature`, and an empty persistent cookie jar.
    pub fn new(config: FingerprintConfig) -> CoreResult<Self> {
        let user_agent = config.browser_user_agent();
        let browser_version = config.browser_version();
        let sec_ch_ua = config.sec_ch_ua();
        let launch_id = Uuid::new_v4().to_string();
        let heartbeat_session_id = Uuid::new_v4().to_string();
        let launch_signature = generate_launch_signature();

        let identify_properties = IdentifyProperties {
            os: config.os_name.clone(),
            browser: config.browser_name.clone(),
            device: String::new(),
            system_locale: config.locale.clone(),
            has_client_mods: false,
            browser_user_agent: user_agent.clone(),
            browser_version: browser_version.clone(),
            os_version: config.os_version.clone(),
            referrer: String::new(),
            referring_domain: String::new(),
            referrer_current: String::new(),
            referring_domain_current: String::new(),
            release_channel: config.release_channel.clone(),
            client_build_number: config.client_build_number,
            client_event_source: None,
            client_launch_id: launch_id.clone(),
            is_fast_connect: true,
        };

        let super_properties = SuperProperties {
            os: config.os_name.clone(),
            browser: config.browser_name.clone(),
            device: String::new(),
            system_locale: config.locale.clone(),
            has_client_mods: false,
            browser_user_agent: user_agent.clone(),
            browser_version,
            os_version: config.os_version.clone(),
            referrer: String::new(),
            referring_domain: String::new(),
            referrer_current: String::new(),
            referring_domain_current: String::new(),
            release_channel: config.release_channel.clone(),
            client_build_number: config.client_build_number,
            client_event_source: None,
            client_launch_id: launch_id,
            launch_signature,
            client_heartbeat_session_id: heartbeat_session_id,
            client_app_state: "focused".to_owned(),
        };

        let super_properties_b64 = super_properties.to_base64()?;

        Ok(Self {
            config,
            user_agent,
            sec_ch_ua,
            identify_properties,
            super_properties,
            super_properties_b64,
            cookie_jar: Arc::new(Jar::default()),
        })
    }

    /// Build a fingerprint using [`FingerprintConfig::default`].
    pub fn default_chrome() -> CoreResult<Self> {
        Self::new(FingerprintConfig::default())
    }

    /// Return the underlying [`FingerprintConfig`].
    #[must_use]
    pub const fn config(&self) -> &FingerprintConfig {
        &self.config
    }

    /// Return the derived Chrome `User-Agent` string.
    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Return the derived GREASE `Sec-Ch-Ua` header string.
    #[must_use]
    pub fn sec_ch_ua(&self) -> &str {
        &self.sec_ch_ua
    }

    /// Return the Gateway `IDENTIFY` properties.
    #[must_use]
    pub const fn identify_properties(&self) -> &IdentifyProperties {
        &self.identify_properties
    }

    /// Return the REST `X-Super-Properties` struct.
    #[must_use]
    pub const fn super_properties(&self) -> &SuperProperties {
        &self.super_properties
    }

    /// Return the Base64-encoded `X-Super-Properties` header value.
    #[must_use]
    pub fn super_properties_base64(&self) -> &str {
        &self.super_properties_b64
    }

    /// Return the shared persistent cookie jar used across requests.
    #[must_use]
    pub fn cookie_jar(&self) -> Arc<Jar> {
        Arc::clone(&self.cookie_jar)
    }

    /// Build the default Discord REST header map matching `discordo/internal/http/headers.go`.
    pub fn build_rest_headers(&self) -> CoreResult<HeaderMap> {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, header::USER_AGENT, &self.user_agent)?;
        insert_header(&mut headers, header::ACCEPT, "*/*")?;
        insert_header(&mut headers, header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")?;
        insert_header(&mut headers, header::ORIGIN, DISCORD_ORIGIN)?;
        insert_custom_header(&mut headers, "priority", "u=1, i")?;
        insert_header(&mut headers, header::REFERER, DISCORD_REFERER)?;

        insert_custom_header(&mut headers, "sec-fetch-dest", "empty")?;
        insert_custom_header(&mut headers, "sec-fetch-mode", "cors")?;
        insert_custom_header(&mut headers, "sec-fetch-site", "same-origin")?;

        insert_custom_header(&mut headers, "sec-ch-ua", &self.sec_ch_ua)?;
        insert_custom_header(&mut headers, "sec-ch-ua-mobile", "?0")?;
        let platform = format!("\"{}\"", self.config.os_name);
        insert_custom_header(&mut headers, "sec-ch-ua-platform", &platform)?;

        insert_custom_header(&mut headers, "x-debug-options", "bugReporterEnabled")?;
        insert_custom_header(&mut headers, "x-discord-locale", &self.config.locale)?;
        if !self.config.timezone.is_empty() {
            insert_custom_header(&mut headers, "x-discord-timezone", &self.config.timezone)?;
        }
        insert_custom_header(
            &mut headers,
            "x-super-properties",
            &self.super_properties_b64,
        )?;

        Ok(headers)
    }

    /// Build the Chrome TLS + HTTP/2 [`EmulationProvider`] for REST requests.
    pub fn build_chrome_rest_emulation(&self) -> CoreResult<EmulationProvider> {
        let tls = TlsConfig::builder()
            .curves(CHROME_CURVES)
            .cipher_list(CHROME_CIPHER_LIST)
            .sigalgs_list(CHROME_SIGALGS_LIST)
            .cert_compression_algorithm(CHROME_CERT_COMPRESSION)
            .grease_enabled(true)
            .enable_ocsp_stapling(true)
            .enable_signed_cert_timestamps(true)
            .permute_extensions(true)
            .pre_shared_key(true)
            .enable_ech_grease(true)
            .alpn_protos(AlpnProtos::ALL)
            .alps_protos(AlpsProtos::HTTP2)
            .alps_use_new_codepoint(true)
            .aes_hw_override(true)
            .min_tls_version(TlsVersion::TLS_1_2)
            .max_tls_version(TlsVersion::TLS_1_3)
            .build();

        let http1 = Http1Config::builder().build();

        let http2 = Http2Config::builder()
            .header_table_size(65_536)
            .enable_push(false)
            .initial_stream_window_size(6_291_456)
            .max_header_list_size(262_144)
            .initial_connection_window_size(15_728_640)
            .headers_priority(StreamDependency::new(StreamId::zero(), 219, true))
            .headers_pseudo_order([
                PseudoOrder::Method,
                PseudoOrder::Authority,
                PseudoOrder::Scheme,
                PseudoOrder::Path,
            ])
            .settings_order([
                SettingsOrder::HeaderTableSize,
                SettingsOrder::EnablePush,
                SettingsOrder::MaxConcurrentStreams,
                SettingsOrder::InitialWindowSize,
                SettingsOrder::MaxFrameSize,
                SettingsOrder::MaxHeaderListSize,
                SettingsOrder::UnknownSetting8,
                SettingsOrder::UnknownSetting9,
            ])
            .build();

        let default_headers = self.build_rest_headers()?;

        Ok(EmulationProvider::builder()
            .tls_config(tls)
            .http1_config(http1)
            .http2_config(http2)
            .default_headers(default_headers)
            .headers_order(CHROME_HEADER_ORDER)
            .build())
    }

    /// Build a Chrome-impersonating `wreq::Client` with persistent cookie jar for Discord REST.
    pub fn build_rest_client(&self) -> CoreResult<Client> {
        let emulation = self.build_chrome_rest_emulation()?;
        Client::builder()
            .emulation(emulation)
            .cookie_provider(Arc::clone(&self.cookie_jar))
            .connect_timeout(self.config.connect_timeout)
            .timeout(self.config.request_timeout)
            .build()
            .map_err(|err| CoreError::Network(format!("build wreq REST client: {err}")))
    }

    /// Build a lightweight Chrome-impersonating `wreq::Client` for third-party / CDN fetches
    /// without injecting Discord-only `X-Super-Properties` or `Origin: https://discord.com` headers.
    pub fn build_plain_browser_client(&self) -> CoreResult<Client> {
        let tls = TlsConfig::builder()
            .curves(CHROME_CURVES)
            .cipher_list(CHROME_CIPHER_LIST)
            .sigalgs_list(CHROME_SIGALGS_LIST)
            .cert_compression_algorithm(CHROME_CERT_COMPRESSION)
            .grease_enabled(true)
            .enable_ocsp_stapling(true)
            .enable_signed_cert_timestamps(true)
            .permute_extensions(true)
            .pre_shared_key(true)
            .enable_ech_grease(true)
            .alpn_protos(AlpnProtos::ALL)
            .alps_protos(AlpsProtos::HTTP2)
            .alps_use_new_codepoint(true)
            .aes_hw_override(true)
            .min_tls_version(TlsVersion::TLS_1_2)
            .max_tls_version(TlsVersion::TLS_1_3)
            .build();

        let mut headers = HeaderMap::new();
        insert_header(&mut headers, header::USER_AGENT, &self.user_agent)?;
        insert_header(&mut headers, header::ACCEPT, "*/*")?;
        insert_header(&mut headers, header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")?;

        let emulation = EmulationProvider::builder()
            .tls_config(tls)
            .default_headers(headers)
            .build();

        Client::builder()
            .emulation(emulation)
            .connect_timeout(self.config.connect_timeout)
            .timeout(self.config.request_timeout)
            .build()
            .map_err(|err| CoreError::Network(format!("build wreq browser client: {err}")))
    }

    /// Build a Chrome TLS `wreq::Client` forced to HTTP/1.1 ALPN for Discord Gateway and
    /// Remote-Auth WebSocket upgrades (matching `discordo/internal/gateway/dialer.go`).
    pub fn build_websocket_client(&self) -> CoreResult<Client> {
        let tls = TlsConfig::builder()
            .curves(CHROME_CURVES)
            .cipher_list(CHROME_CIPHER_LIST)
            .sigalgs_list(CHROME_SIGALGS_LIST)
            .cert_compression_algorithm(CHROME_CERT_COMPRESSION)
            .grease_enabled(true)
            .enable_ocsp_stapling(true)
            .enable_signed_cert_timestamps(true)
            .permute_extensions(true)
            .pre_shared_key(true)
            .enable_ech_grease(true)
            .alpn_protos(AlpnProtos::HTTP1)
            .aes_hw_override(true)
            .min_tls_version(TlsVersion::TLS_1_2)
            .max_tls_version(TlsVersion::TLS_1_3)
            .build();

        let http1 = Http1Config::builder().title_case_headers(true).build();

        let mut default_headers = HeaderMap::new();
        insert_header(&mut default_headers, header::PRAGMA, "no-cache")?;
        insert_header(&mut default_headers, header::CACHE_CONTROL, "no-cache")?;
        insert_header(&mut default_headers, header::USER_AGENT, &self.user_agent)?;
        insert_header(&mut default_headers, header::ORIGIN, DISCORD_ORIGIN)?;
        insert_header(
            &mut default_headers,
            header::ACCEPT_ENCODING,
            "gzip, deflate, br",
        )?;
        insert_header(
            &mut default_headers,
            header::ACCEPT_LANGUAGE,
            "en-US,en;q=0.9",
        )?;

        let emulation = EmulationProvider::builder()
            .tls_config(tls)
            .http1_config(http1)
            .default_headers(default_headers)
            .headers_order(CHROME_WS_HEADER_ORDER)
            .build();

        Client::builder()
            .emulation(emulation)
            .cookie_provider(Arc::clone(&self.cookie_jar))
            .connect_timeout(self.config.connect_timeout)
            .build()
            .map_err(|err| CoreError::Network(format!("build wreq WebSocket client: {err}")))
    }
}

fn insert_header(headers: &mut HeaderMap, name: HeaderName, value: &str) -> CoreResult<()> {
    let val = HeaderValue::from_str(value)
        .map_err(|err| CoreError::Protocol(format!("invalid header value for {name}: {err}")))?;
    headers.insert(name, val);
    Ok(())
}

fn insert_custom_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> CoreResult<()> {
    let header_name = HeaderName::from_static(name);
    insert_header(headers, header_name, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grease_sec_ch_ua_matches_discordo_and_chromium_vectors() {
        // Exact vectors from discordo/internal/http/properties_test.go:80-92
        assert_eq!(
            sec_ch_ua_brands("Chrome", 150),
            r#""Not;A=Brand";v="8", "Chromium";v="150", "Google Chrome";v="150""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 156),
            r#""Not:A-Brand";v="8", "Chromium";v="156", "Google Chrome";v="156""#
        );

        // Additional real Chromium release vectors (verified against Chromium user_agent_utils.cc)
        assert_eq!(
            sec_ch_ua_brands("Chrome", 105),
            r#""Google Chrome";v="105", "Not)A;Brand";v="8", "Chromium";v="105""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 106),
            r#""Chromium";v="106", "Google Chrome";v="106", "Not;A=Brand";v="99""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 114),
            r#""Not.A/Brand";v="8", "Chromium";v="114", "Google Chrome";v="114""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 120),
            r#""Not_A Brand";v="8", "Chromium";v="120", "Google Chrome";v="120""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 124),
            r#""Chromium";v="124", "Google Chrome";v="124", "Not-A.Brand";v="99""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 128),
            r#""Chromium";v="128", "Not;A=Brand";v="24", "Google Chrome";v="128""#
        );
        assert_eq!(
            sec_ch_ua_brands("Chrome", 152),
            r#""Chromium";v="152", "Not?A_Brand";v="24", "Google Chrome";v="152""#
        );
    }

    #[test]
    fn launch_signature_preserves_uuid_v4_and_clears_detection_bits() {
        // Test worst-case all-1s input as well as 1,000 random generated signatures.
        let all_ones = apply_launch_signature_mask([0xFF; 16]);
        let all_ones_uuid = Uuid::from_bytes(all_ones);
        assert_eq!(all_ones_uuid.get_version_num(), 4);
        assert_eq!(all_ones[8] >> 6, 0b10);
        for (i, &mask_byte) in LAUNCH_SIGNATURE_MASK.iter().enumerate() {
            assert_eq!(all_ones[i] & !mask_byte, 0, "detection bit leaked at byte {i}");
        }

        for _ in 0..1_000 {
            let sig = generate_launch_signature();
            let parsed = Uuid::parse_str(&sig).expect("valid UUID");
            let bytes = parsed.as_bytes();
            assert_eq!(bytes[6] >> 4, 4, "UUIDv4 version nibble must remain 4");
            assert_eq!(bytes[8] >> 6, 2, "UUID RFC 4122 variant bits must remain 0b10");
            for (i, &mask_byte) in LAUNCH_SIGNATURE_MASK.iter().enumerate() {
                assert_eq!(
                    bytes[i] & !mask_byte,
                    0,
                    "detection bit set at byte {i} for {sig}"
                );
            }
        }
    }

    #[test]
    fn super_properties_base64_and_headers_match_discordo_contract() {
        let cfg = FingerprintConfig {
            timezone: "America/Chicago".to_owned(),
            ..Default::default()
        };
        let fp = DiscordFingerprint::new(cfg).expect("fingerprint");
        let headers = fp.build_rest_headers().expect("headers");

        assert_eq!(
            headers.get("sec-ch-ua").and_then(|v| v.to_str().ok()),
            Some(sec_ch_ua_brands("Chrome", DEFAULT_BROWSER_MAJOR_VERSION).as_str())
        );
        assert_eq!(
            headers.get("sec-ch-ua-mobile").and_then(|v| v.to_str().ok()),
            Some("?0")
        );
        assert_eq!(
            headers.get("sec-ch-ua-platform").and_then(|v| v.to_str().ok()),
            Some("\"Windows\"")
        );
        assert_eq!(
            headers.get("x-debug-options").and_then(|v| v.to_str().ok()),
            Some("bugReporterEnabled")
        );
        assert_eq!(
            headers.get("x-discord-locale").and_then(|v| v.to_str().ok()),
            Some("en-US")
        );
        assert_eq!(
            headers.get("x-discord-timezone").and_then(|v| v.to_str().ok()),
            Some("America/Chicago")
        );
        assert_eq!(
            headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
            Some("https://discord.com")
        );
        assert_eq!(
            headers.get(header::REFERER).and_then(|v| v.to_str().ok()),
            Some("https://discord.com/channels/@me")
        );
        assert_eq!(
            headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()),
            Some("cors")
        );
        assert_eq!(
            headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()),
            Some("same-origin")
        );

        let b64 = headers
            .get("x-super-properties")
            .and_then(|v| v.to_str().ok())
            .expect("x-super-properties");
        let decoded = SuperProperties::from_base64(b64).expect("decode super props");
        assert_eq!(decoded.os, "Windows");
        assert_eq!(decoded.browser, "Chrome");
        assert_eq!(decoded.client_app_state, "focused");
        assert!(!decoded.has_client_mods);
        assert!(Uuid::parse_str(&decoded.client_launch_id).is_ok());
        assert!(Uuid::parse_str(&decoded.client_heartbeat_session_id).is_ok());
        assert!(Uuid::parse_str(&decoded.launch_signature).is_ok());

        // Ensure both REST and WS wreq clients construct cleanly.
        assert!(fp.build_rest_client().is_ok());
        assert!(fp.build_websocket_client().is_ok());
        assert!(fp.build_plain_browser_client().is_ok());
    }

    #[test]
    fn zone_from_localtime_link_parses_standard_zoneinfo_paths() {
        assert_eq!(
            zone_from_localtime_link("/usr/share/zoneinfo/America/New_York").as_deref(),
            Some("America/New_York")
        );
        assert_eq!(
            zone_from_localtime_link("../usr/share/zoneinfo/Europe/Berlin").as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(
            zone_from_localtime_link("../../usr/share/zoneinfo/Asia/Tokyo").as_deref(),
            Some("Asia/Tokyo")
        );
        assert_eq!(zone_from_localtime_link("/etc/something-else"), None);
    }
}
