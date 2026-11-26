//! Comprehensive 100% offline wire & state-machine test suite for `bbs-protocol`.
//!
//! Spins up ephemeral `127.0.0.1:0` HTTP and WebSocket servers inside Tokio tasks so every
//! wire path (rate-limit 429 retry, CDN refresh, Group DM CRUD, call signaling, multipart upload,
//! zlib-stream Gateway v10, QR Remote-Auth RSA-OAEP, and Discord/Klipy/Giphy GIF adapters) is
//! verified deterministically without touching the public internet or any real credentials.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use bbs_core::{
    BackendEvent, CallRingState, ChannelKind, ConnectionState, CoreError, DiscordBackend,
    GifProvider, GifProviderKind, GifSearchQuery, SecretToken, Snowflake,
};
use bbs_protocol::gateway::{
    DiscordGateway, GatewayCommand, GatewayConfig, GatewaySessionState, GatewaySignalingEvent,
    GuildSubscriptionPayload, ZlibStreamInflater, capabilities, normalize_gateway_url, opcodes,
    parse_stream_key, serialize_gateway_command,
};
use bbs_protocol::gifs::{
    CompositeGifProvider, DiscordGifProvider, GiphyGifProvider, KlipyGifProvider,
    is_direct_gif_or_webp_url, parse_discord_gifs_response, parse_giphy_response,
    parse_klipy_response, probe_direct_gif_url, resolve_direct_gif_url,
};
use bbs_protocol::remote_auth::{RemoteAuthClient, RemoteAuthEvent};
use bbs_protocol::{
    DiscordFingerprint, DiscordRestClient, FingerprintConfig, RateLimitConfig, UploadAttachment,
    encode_reaction_emoji, is_discord_cdn_url, preserve_cdn_resize_params,
};
use flate2::{Compress, Compression, FlushCompress};
use futures::{SinkExt, StreamExt};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ============================================================================
// Minimal Local HTTP/1.1 Mock Server Helper (zero external dev-deps)
// ============================================================================

#[derive(Debug, Clone)]
struct CapturedHttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct MockHttpResponse {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl MockHttpResponse {
    fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            reason: status_reason(status),
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: body.into().into_bytes(),
        }
    }

    fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason: status_reason(status),
            headers: vec![("Content-Type".to_owned(), content_type.to_owned())],
            body,
        }
    }

    fn no_content() -> Self {
        Self {
            status: 204,
            reason: "No Content",
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<CapturedHttpRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head_str = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head_str.split("\r\n");
    let req_line = lines.next()?;
    let mut req_parts = req_line.split_whitespace();
    let method = req_parts.next()?.to_owned();
    let target = req_parts.next()?.to_owned();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_owned();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Some(CapturedHttpRequest {
        method,
        target,
        headers,
        body,
    })
}

async fn write_http_response(stream: &mut tokio::net::TcpStream, resp: &MockHttpResponse) {
    let mut raw = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.reason,
        resp.body.len()
    );
    for (k, v) in &resp.headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str("\r\n");
    let _ = stream.write_all(raw.as_bytes()).await;
    if !resp.body.is_empty() {
        let _ = stream.write_all(&resp.body).await;
    }
    let _ = stream.flush().await;
}

async fn spawn_mock_http_server<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: Fn(CapturedHttpRequest) -> MockHttpResponse + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handler = Arc::new(handler);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let h = Arc::clone(&handler);
            tokio::spawn(async move {
                if let Some(req) = read_http_request(&mut stream).await {
                    let resp = h(req);
                    write_http_response(&mut stream, &resp).await;
                }
            });
        }
    });
    (format!("http://{addr}"), task)
}

fn compress_zlib_stream_frame(compressor: &mut Compress, plaintext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(plaintext.len() + 64);
    let mut chunk = [0u8; 4096];
    let mut offset = 0;

    // Feed all input, then flush with Z_SYNC_FLUSH so the trailing 0x00 0x00 0xFF 0xFF appears.
    loop {
        let before_in = compressor.total_in();
        let before_out = compressor.total_out();
        let flush = if offset >= plaintext.len() {
            FlushCompress::Sync
        } else {
            FlushCompress::None
        };
        let _ = compressor
            .compress(&plaintext[offset..], &mut chunk, flush)
            .expect("zlib compress");
        let consumed = (compressor.total_in() - before_in) as usize;
        let produced = (compressor.total_out() - before_out) as usize;
        offset += consumed;
        out.extend_from_slice(&chunk[..produced]);
        if offset >= plaintext.len() && out.ends_with(&[0x00, 0x00, 0xFF, 0xFF]) {
            break;
        }
    }
    out
}

fn test_fingerprint() -> DiscordFingerprint {
    DiscordFingerprint::new(FingerprintConfig {
        timezone: "UTC".to_owned(),
        ..Default::default()
    })
    .expect("test fingerprint")
}

// ============================================================================
// 1. Zlib-Stream Framing & Safety Tests
// ============================================================================

#[test]
fn zlib_stream_inflater_handles_fragmented_and_consecutive_stateful_frames() {
    let mut compressor = Compress::new(Compression::default(), true);
    let mut inflater = ZlibStreamInflater::default();

    let msg1 = br#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
    let msg2 = br#"{"op":0,"s":1,"t":"MESSAGE_CREATE","d":{"id":"100","channel_id":"200","content":"hello zlib-stream window reuse hello zlib-stream"}}"#;

    let frame1 = compress_zlib_stream_frame(&mut compressor, msg1);
    let frame2 = compress_zlib_stream_frame(&mut compressor, msg2);

    // Split frame1 into 2 fragments to verify incomplete buffering.
    let mid = frame1.len() / 2;
    assert_eq!(inflater.push_chunk(&frame1[..mid]).expect("part1"), None);
    let decoded1 = inflater
        .push_chunk(&frame1[mid..])
        .expect("part2")
        .expect("complete msg1");
    assert_eq!(decoded1, msg1);

    // Second message reuses the continuous sliding dictionary from frame1.
    let decoded2 = inflater
        .push_chunk(&frame2)
        .expect("frame2")
        .expect("complete msg2");
    assert_eq!(decoded2, msg2);
}

#[test]
fn zlib_stream_inflater_handles_large_high_ratio_payload_without_truncation() {
    let mut compressor = Compress::new(Compression::default(), true);
    let mut inflater = ZlibStreamInflater::default();

    // Build a >100 KiB highly compressible JSON payload (compresses to < 2 KiB, expands across
    // >12 iterations of the 8 KiB scratch buffer after all compressed bytes are consumed!).
    let mut channels = Vec::new();
    for i in 0..1_200u64 {
        channels.push(serde_json::json!({
            "id": format!("{}", 50_000 + i),
            "type": 0,
            "name": "repeated-channel-name-for-high-compression-ratio",
            "topic": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }));
    }
    let big_ready = serde_json::json!({
        "op": 0,
        "s": 1,
        "t": "READY",
        "d": { "channels": channels }
    })
    .to_string();
    assert!(big_ready.len() > 100_000);

    let frame1 = compress_zlib_stream_frame(&mut compressor, big_ready.as_bytes());
    assert!(
        frame1.len() < 16_384,
        "expected high compression ratio for large READY test"
    );
    let decoded1 = inflater
        .push_chunk(&frame1)
        .expect("inflate large frame")
        .expect("complete large frame");
    assert_eq!(decoded1, big_ready.as_bytes());

    // Immediately follow with another compressed frame on the same zlib stream to verify stream integrity.
    let follow_up = br#"{"op":11}"#;
    let frame2 = compress_zlib_stream_frame(&mut compressor, follow_up);
    let decoded2 = inflater
        .push_chunk(&frame2)
        .expect("inflate follow-up")
        .expect("complete follow-up");
    assert_eq!(decoded2, follow_up);
}

#[tokio::test]
async fn rate_limiter_scopes_bucket_hashes_by_channel_major_parameter() {
    let limiter = bbs_protocol::RateLimiter::new();
    let mut headers = http::HeaderMap::new();
    headers.insert("x-ratelimit-bucket", http::HeaderValue::from_static("shared-template-hash"));
    headers.insert("x-ratelimit-remaining", http::HeaderValue::from_static("0"));
    headers.insert("x-ratelimit-reset-after", http::HeaderValue::from_static("5.0"));

    limiter
        .observe_headers("POST /channels/111/messages", &headers)
        .await;

    // Channel 111 must wait; channel 222 with the same template bucket hash must NOT be blocked.
    assert!(
        limiter
            .pending_wait("POST /channels/111/messages")
            .await
            .is_some()
    );
    assert!(
        limiter
            .pending_wait("POST /channels/222/messages")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn gateway_reconnects_and_resumes_on_heartbeat_ack_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ack ws");
    let ws_addr = listener.local_addr().expect("ack ws addr");
    let ws_url = format!("ws://{ws_addr}/?v=10&encoding=json&compress=zlib-stream");
    let resumed_count = Arc::new(AtomicUsize::new(0));
    let resumed_clone = Arc::clone(&resumed_count);

    let ws_server = tokio::spawn(async move {
        // Conn 1: send HELLO (250ms), receive IDENTIFY, send READY, then IGNORE heartbeat (triggering ACK timeout!).
        if let Ok((stream, _)) = listener.accept().await {
            let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws1");
            let mut comp = Compress::new(Compression::default(), true);
            let hello = serde_json::json!({ "op": 10, "d": { "heartbeat_interval": 250 } });
            ws.send(WsMessage::Binary(
                compress_zlib_stream_frame(&mut comp, hello.to_string().as_bytes()).into(),
            ))
            .await
            .expect("hello");
            let _ = ws.next().await; // IDENTIFY
            let ready = serde_json::json!({
                "op": 0, "s": 1, "t": "READY",
                "d": {
                    "session_id": "zombie-sess-1",
                    "resume_gateway_url": format!("ws://{ws_addr}"),
                    "user": {"id": "1001", "username": "sysop"}
                }
            });
            ws.send(WsMessage::Binary(
                compress_zlib_stream_frame(&mut comp, ready.to_string().as_bytes()).into(),
            ))
            .await
            .expect("ready");

            // Read first heartbeat and deliberately do NOT send Op 11 HEARTBEAT_ACK!
            let _ = ws.next().await;
            // Wait until client closes zombie connection.
            let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
        }

        // Conn 2: verify client reconnects and sends Op 6 RESUME!
        if let Ok((stream, _)) = listener.accept().await {
            let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws2");
            let mut comp = Compress::new(Compression::default(), true);
            let hello = serde_json::json!({ "op": 10, "d": { "heartbeat_interval": 30_000 } });
            ws.send(WsMessage::Binary(
                compress_zlib_stream_frame(&mut comp, hello.to_string().as_bytes()).into(),
            ))
            .await
            .expect("hello2");
            if let Some(Ok(frame)) = ws.next().await
                && let Ok(txt) = frame.to_text()
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(txt)
                && val["op"] == 6
                && val["d"]["session_id"] == "zombie-sess-1"
            {
                resumed_clone.fetch_add(1, Ordering::SeqCst);
                let resumed = serde_json::json!({ "op": 0, "s": 2, "t": "RESUMED", "d": {} });
                let _ = ws
                    .send(WsMessage::Binary(
                        compress_zlib_stream_frame(&mut comp, resumed.to_string().as_bytes()).into(),
                    ))
                    .await;
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
        }
    });

    let (events_tx, mut events_rx) = broadcast::channel(64);
    let token = SecretToken::new("offline-test-session-cred").expect("token");
    let cfg = GatewayConfig {
        gateway_url: ws_url,
        capabilities: capabilities::DEFAULT_USER_CAPABILITIES,
        initial_reconnect_backoff: Duration::from_millis(20),
        max_reconnect_backoff: Duration::from_millis(80),
        heartbeat_jitter_override: Some(0.2),
    };
    let (gw, handle) = DiscordGateway::new(token, test_fingerprint(), cfg, events_tx);
    let gw_task = tokio::spawn(gw.run());

    let mut saw_resumed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(BackendEvent::ConnectionChanged {
            state: ConnectionState::Ready,
            detail,
            ..
        })) = tokio::time::timeout(Duration::from_millis(300), events_rx.recv()).await
            && detail.as_deref() == Some("session resumed")
        {
            saw_resumed = true;
            break;
        }
    }

    handle.shutdown().expect("shutdown");
    let _ = tokio::time::timeout(Duration::from_secs(2), gw_task).await;
    assert!(saw_resumed, "expected zombie heartbeat ACK timeout to trigger RESUME");
    assert_eq!(resumed_count.load(Ordering::SeqCst), 1);

    ws_server.abort();
}

#[test]
fn zlib_stream_inflater_rejects_oversized_and_corrupt_payloads_without_panicking() {
    let mut compressor = Compress::new(Compression::default(), true);
    let big = vec![b'A'; 4096];
    let frame = compress_zlib_stream_frame(&mut compressor, &big);

    let mut tiny_cap = ZlibStreamInflater::new(256);
    let err = tiny_cap
        .push_chunk(&frame)
        .expect_err("must reject payload exceeding cap");
    assert!(matches!(err, CoreError::Protocol(_)));

    let mut inflater = ZlibStreamInflater::default();
    let corrupt = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0xFF, 0xFF];
    assert!(inflater.push_chunk(&corrupt).is_err());
}

// ============================================================================
// 2. REST Rate-Limit 429 Retry, Group DM CRUD, Calls, Multipart, CDN Refresh
// ============================================================================

#[tokio::test]
async fn rest_handles_429_retry_after_and_bucket_headers() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = Arc::clone(&attempts);

    let (base_url, server) = spawn_mock_http_server(move |req| {
        assert_eq!(req.method, "GET");
        assert_eq!(req.target, "/api/v10/users/@me");
        assert_eq!(
            req.headers.get("authorization").map(String::as_str),
            Some("offline-test-session-cred")
        );
        assert!(req.headers.contains_key("x-super-properties"));
        assert!(req.headers.contains_key("sec-ch-ua"));
        assert_eq!(
            req.headers.get("x-debug-options").map(String::as_str),
            Some("bugReporterEnabled")
        );

        let n = attempts_clone.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            MockHttpResponse::json(
                429,
                r#"{"message":"You are being rate limited.","retry_after":0.05,"global":false}"#,
            )
            .with_header("Retry-After", "0.05")
            .with_header("X-RateLimit-Bucket", "bucket-users-me")
            .with_header("X-RateLimit-Remaining", "0")
            .with_header("X-RateLimit-Reset-After", "0.05")
        } else {
            MockHttpResponse::json(
                200,
                r#"{"id":"100000000000000001","username":"bbs_sysop","global_name":"BBS SysOp","discriminator":"0"}"#,
            )
            .with_header("X-RateLimit-Bucket", "bucket-users-me")
            .with_header("X-RateLimit-Remaining", "4")
            .with_header("X-RateLimit-Reset-After", "1.0")
        }
    })
    .await;

    let token = SecretToken::new("offline-test-session-cred").expect("token");
    let client = DiscordRestClient::new(token, test_fingerprint())
        .expect("client")
        .with_api_base_url(format!("{base_url}/api/v10"))
        .with_rate_limit_config(RateLimitConfig {
            max_retries: 2,
            max_retry_wait: Duration::from_secs(5),
        });

    let user = client.validate_token().await.expect("recovered after 429");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(user.id, Snowflake(100_000_000_000_000_001));
    assert_eq!(user.username, "bbs_sysop");
    assert_eq!(user.display_name(), "BBS SysOp");
    assert_eq!(user.discriminator, None);

    server.abort();
}

#[tokio::test]
async fn rest_group_dm_crud_call_signaling_multipart_and_messages_wire_contract() {
    let captured = Arc::new(Mutex::new(Vec::<CapturedHttpRequest>::new()));
    let captured_clone = Arc::clone(&captured);

    let (base_url, server) = spawn_mock_http_server(move |req| {
        let target = req.target.clone();
        let method = req.method.clone();
        if let Ok(mut guard) = captured_clone.try_lock() {
            guard.push(req.clone());
        }

        match (method.as_str(), target.as_str()) {
            // Create Group DM (type = 3)
            ("POST", "/api/v10/users/@me/channels") => MockHttpResponse::json(
                200,
                r#"{
                    "id": "500000000000000003",
                    "type": 3,
                    "name": null,
                    "owner_id": "100000000000000001",
                    "recipients": [
                        {"id": "100000000000000002", "username": "alice"},
                        {"id": "100000000000000003", "username": "bob"}
                    ]
                }"#,
            ),
            // Rename Group DM
            ("PATCH", "/api/v10/channels/500000000000000003") => MockHttpResponse::json(
                200,
                r#"{
                    "id": "500000000000000003",
                    "type": 3,
                    "name": "Retro Ops",
                    "owner_id": "100000000000000001",
                    "recipients": [
                        {"id": "100000000000000002", "username": "alice"},
                        {"id": "100000000000000003", "username": "bob"}
                    ]
                }"#,
            ),
            // Add Group DM recipient (returns 204 -> client fetches GET /channels/{id})
            ("PUT", "/api/v10/channels/500000000000000003/recipients/100000000000000004") => {
                MockHttpResponse::no_content()
            }
            ("GET", "/api/v10/channels/500000000000000003") => MockHttpResponse::json(
                200,
                r#"{
                    "id": "500000000000000003",
                    "type": 3,
                    "name": "Retro Ops",
                    "owner_id": "100000000000000001",
                    "recipients": [
                        {"id": "100000000000000002", "username": "alice"},
                        {"id": "100000000000000003", "username": "bob"},
                        {"id": "100000000000000004", "username": "carol"}
                    ]
                }"#,
            ),
            // Remove Group DM recipient (returning updated channel directly)
            ("DELETE", "/api/v10/channels/500000000000000003/recipients/100000000000000004") => {
                MockHttpResponse::json(
                    200,
                    r#"{
                        "id": "500000000000000003",
                        "type": 3,
                        "name": "Retro Ops",
                        "owner_id": "100000000000000001",
                        "recipients": [
                            {"id": "100000000000000002", "username": "alice"},
                            {"id": "100000000000000003", "username": "bob"}
                        ]
                    }"#,
                )
            }
            // Ring / Stop ringing Group DM call
            ("POST", "/api/v10/channels/500000000000000003/call/ring")
            | ("POST", "/api/v10/channels/500000000000000003/call/stop-ringing")
            | ("POST", "/api/v10/channels/500000000000000003/typing")
            | ("DELETE", "/api/v10/channels/500000000000000003?silent=false") => {
                MockHttpResponse::no_content()
            }
            // Fetch messages (Discord newest-first -> client reverses to chronological)
            ("GET", "/api/v10/channels/500000000000000003/messages?limit=25") => {
                MockHttpResponse::json(
                    200,
                    r#"[
                        {"id":"202","channel_id":"500000000000000003","author":{"id":"100000000000000002","username":"alice"},"content":"newer","timestamp":"2026-11-26T00:01:00Z"},
                        {"id":"201","channel_id":"500000000000000003","author":{"id":"100000000000000001","username":"sysop"},"content":"older","timestamp":"2026-11-26T00:00:00Z"}
                    ]"#,
                )
            }
            // Multipart attachment upload
            ("POST", "/api/v10/channels/500000000000000003/messages") => MockHttpResponse::json(
                200,
                r#"{
                    "id":"203",
                    "channel_id":"500000000000000003",
                    "author":{"id":"100000000000000001","username":"sysop"},
                    "content":"animated hello",
                    "timestamp":"2026-11-26T00:02:00Z",
                    "attachments":[{
                        "id":"9001",
                        "filename":"wave.gif",
                        "content_type":"image/gif",
                        "size":12,
                        "url":"https://cdn.discordapp.com/attachments/1/2/wave.gif?ex=67452301"
                    }]
                }"#,
            ),
            // Reaction add / remove
            ("PUT", p) | ("DELETE", p) if p.contains("/reactions/") => {
                MockHttpResponse::no_content()
            }
            _ => MockHttpResponse::json(404, r#"{"message":"not found"}"#),
        }
    })
    .await;

    let token = SecretToken::new("offline-test-session-cred").expect("token");
    let client = DiscordRestClient::new(token, test_fingerprint())
        .expect("client")
        .with_api_base_url(format!("{base_url}/api/v10"));

    // Group DM full CRUD
    let gdm = client
        .create_group_dm(
            &[
                Snowflake(100_000_000_000_000_002),
                Snowflake(100_000_000_000_000_003),
            ],
            Some("Retro Ops"),
        )
        .await
        .expect("create group dm");
    assert_eq!(gdm.kind, ChannelKind::GroupDm);
    assert_eq!(gdm.name, "Retro Ops");
    assert_eq!(gdm.recipients.len(), 2);

    let gdm = client
        .add_group_dm_recipient(gdm.id, Snowflake(100_000_000_000_000_004))
        .await
        .expect("add recipient");
    assert_eq!(gdm.recipients.len(), 3);

    let gdm = client
        .remove_group_dm_recipient(gdm.id, Snowflake(100_000_000_000_000_004))
        .await
        .expect("remove recipient");
    assert_eq!(gdm.recipients.len(), 2);

    // DM / Group DM call signaling
    let ring = client
        .ring_call(gdm.id, Some(&[Snowflake(100_000_000_000_000_002)]))
        .await
        .expect("ring call");
    assert!(matches!(
        ring,
        CallRingState::OutgoingRinging { channel_id, .. } if channel_id == gdm.id
    ));
    assert_eq!(
        client.stop_call_ring(gdm.id).await.expect("stop ring"),
        CallRingState::Idle
    );

    // Chronological message order
    let msgs = client
        .fetch_messages(gdm.id, None, 25)
        .await
        .expect("fetch messages");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].id, Snowflake(201));
    assert_eq!(msgs[1].id, Snowflake(202));

    // Multipart GIF upload
    let sent = client
        .send_message_with_attachments(
            gdm.id,
            "animated hello",
            Some(Snowflake(202)),
            &[UploadAttachment {
                filename: "wave.gif".to_owned(),
                content_type: Some("image/gif".to_owned()),
                description: Some("retro wave".to_owned()),
                bytes: b"GIF89a......".to_vec(),
            }],
        )
        .await
        .expect("multipart send");
    assert_eq!(sent.attachments.len(), 1);
    assert!(sent.attachments[0].is_gif());
    assert_eq!(sent.attachments[0].expires_at_unix, Some(0x6745_2301));

    // Reactions & typing & leave Group DM
    assert_eq!(encode_reaction_emoji("🔥"), "%F0%9F%94%A5");
    client.add_reaction(gdm.id, sent.id, "🔥").await.expect("add reaction");
    client
        .remove_reaction(gdm.id, sent.id, "🔥")
        .await
        .expect("remove reaction");
    client.send_typing(gdm.id).await.expect("send typing");
    client.leave_group_dm(gdm.id).await.expect("leave group dm");

    // Verify captured multipart body contained `payload_json`, `files[0]`, and `GIF89a......`.
    let reqs = captured.lock().await;
    let multipart_req = reqs
        .iter()
        .find(|r| r.method == "POST" && r.target.ends_with("/messages"))
        .expect("multipart request recorded");
    let ct = multipart_req
        .headers
        .get("content-type")
        .expect("multipart content-type");
    assert!(ct.starts_with("multipart/form-data; boundary="));
    let body_str = String::from_utf8_lossy(&multipart_req.body);
    assert!(body_str.contains("name=\"payload_json\""));
    assert!(body_str.contains("name=\"files[0]\""));
    assert!(body_str.contains("filename=\"wave.gif\""));
    assert!(body_str.contains("GIF89a......"));

    // Verify create_group_dm JSON payload sent `recipients` array.
    let create_req = reqs
        .iter()
        .find(|r| r.method == "POST" && r.target == "/api/v10/users/@me/channels")
        .expect("create group dm request");
    let create_json: serde_json::Value =
        serde_json::from_slice(&create_req.body).expect("create json");
    assert_eq!(
        create_json["recipients"],
        serde_json::json!(["100000000000000002", "100000000000000003"])
    );

    server.abort();
}

#[tokio::test]
async fn rest_refreshes_expired_signed_cdn_url_automatically_on_non_200() {
    let base_holder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let base_for_handler = Arc::clone(&base_holder);

    let (base_url, server) = spawn_mock_http_server(move |req| {
        let base = base_for_handler
            .try_lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        match (req.method.as_str(), req.target.as_str()) {
            ("GET", "/cdn/expired.png?ex=00000001") => {
                MockHttpResponse::bytes(404, "text/plain", b"This content is no longer available.".to_vec())
            }
            ("POST", "/api/v10/attachments/refresh-urls") => {
                let parsed: serde_json::Value =
                    serde_json::from_slice(&req.body).expect("refresh body json");
                let original = parsed["attachment_urls"][0].as_str().unwrap_or("");
                let refreshed = format!("{base}/cdn/fresh.png?ex=6900a1b2&hm=deadbeef");
                MockHttpResponse::json(
                    200,
                    serde_json::json!({
                        "refreshed_urls": [{
                            "original": original,
                            "refreshed": refreshed
                        }]
                    })
                    .to_string(),
                )
            }
            ("GET", "/cdn/fresh.png?ex=6900a1b2&hm=deadbeef") => {
                MockHttpResponse::bytes(200, "image/png", b"\x89PNG\r\n\x1a\nfresh-image".to_vec())
            }
            _ => MockHttpResponse::json(404, r#"{"message":"unexpected route"}"#),
        }
    })
    .await;

    *base_holder.lock().await = base_url.clone();

    let token = SecretToken::new("offline-test-session-cred").expect("token");
    let client = DiscordRestClient::new(token, test_fingerprint())
        .expect("client")
        .with_api_base_url(format!("{base_url}/api/v10"));
    let mut rx = client.subscribe_events();

    let expired_url = format!("{base_url}/cdn/expired.png?ex=00000001");
    let bytes = client
        .fetch_message_attachment_bytes(Snowflake(10), Snowflake(20), Snowflake(30), &expired_url)
        .await
        .expect("auto-refreshed CDN fetch");
    assert_eq!(bytes, b"\x89PNG\r\n\x1a\nfresh-image");

    let ev = rx.recv().await.expect("AttachmentUrlRefreshed event");
    match ev {
        BackendEvent::AttachmentUrlRefreshed {
            channel_id,
            message_id,
            attachment_id,
            url,
            expires_at_unix,
        } => {
            assert_eq!(channel_id, Snowflake(10));
            assert_eq!(message_id, Snowflake(20));
            assert_eq!(attachment_id, Snowflake(30));
            assert!(url.contains("/cdn/fresh.png?ex=6900a1b2"));
            assert_eq!(expires_at_unix, Some(0x6900_a1b2));
        }
        other => panic!("expected AttachmentUrlRefreshed, got {other:?}"),
    }

    server.abort();
}

// ============================================================================
// 3. Gateway v10 State Machine End-to-End Local Wire Test (zlib-stream + RESUME)
// ============================================================================

#[tokio::test]
async fn gateway_v10_zlib_stream_ready_dispatches_commands_heartbeat_and_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws");
    let ws_addr = listener.local_addr().expect("ws addr");
    let ws_url = format!("ws://{ws_addr}/?v=10&encoding=json&compress=zlib-stream");

    let observed_ops = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let observed_clone = Arc::clone(&observed_ops);
    let resume_seen = Arc::new(AtomicUsize::new(0));
    let resume_seen_clone = Arc::clone(&resume_seen);

    let ws_server = tokio::spawn(async move {
        // Connection 1: IDENTIFY -> READY (zlib-stream) -> dispatches -> outbound commands -> RECONNECT
        if let Ok((stream, _)) = listener.accept().await {
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept ws1");
            let mut comp = Compress::new(Compression::default(), true);

            let hello = serde_json::json!({ "op": 10, "d": { "heartbeat_interval": 120 } });
            ws.send(WsMessage::Binary(
                compress_zlib_stream_frame(&mut comp, hello.to_string().as_bytes()).into(),
            ))
            .await
            .expect("send hello");

            // Expect IDENTIFY (op 2)
            let identify_msg = ws.next().await.expect("identify").expect("ok");
            let identify_val: serde_json::Value =
                serde_json::from_str(identify_msg.to_text().expect("text")).expect("json");
            assert_eq!(identify_val["op"], 2);
            assert_eq!(
                identify_val["d"]["capabilities"],
                capabilities::DEFAULT_USER_CAPABILITIES
            );
            assert_eq!(identify_val["d"]["properties"]["os"], "Windows");
            assert_eq!(identify_val["d"]["properties"]["browser"], "Chrome");
            assert_eq!(identify_val["d"]["properties"]["is_fast_connect"], true);
            observed_clone.lock().await.push(identify_val);

            // Send READY (zlib-stream compressed)
            let ready = serde_json::json!({
                "op": 0, "s": 1, "t": "READY",
                "d": {
                    "session_id": "sess-alpha-1",
                    "resume_gateway_url": format!("ws://{ws_addr}"),
                    "user": {"id": "1001", "username": "sysop", "global_name": "SysOp", "discriminator": "0"},
                    "users": [
                        {"id": "1002", "username": "alice", "global_name": "Alice"},
                        {"id": "1003", "username": "bob", "global_name": "Bob"}
                    ],
                    "guilds": [{
                        "id": "2001",
                        "properties": {"name": "RetroBBS", "owner_id": "1001"},
                        "member_count": 42,
                        "channels": [
                            {"id": "3001", "type": 0, "name": "general", "position": 0, "last_message_id": "9999"},
                            {"id": "3002", "type": 2, "name": "voice-lounge", "position": 1}
                        ],
                        "voice_states": [
                            {"guild_id": "2001", "channel_id": "3002", "user_id": "1002", "session_id": "vs-1", "self_mute": false, "self_deaf": false}
                        ]
                    }],
                    "private_channels": [{
                        "id": "4001",
                        "type": 3,
                        "name": "Night Crew",
                        "owner_id": "1001",
                        "recipient_ids": ["1002", "1003"],
                        "last_message_id": "8888"
                    }],
                    "user_settings": {
                        "guild_folders": [
                            {"id": 77, "name": "BBS", "color": 65280, "guild_ids": ["2001"]}
                        ]
                    },
                    "read_state": {
                        "version": 1,
                        "entries": [
                            {"id": "3001", "last_message_id": "9990", "mention_count": 2}
                        ]
                    },
                    "relationships": [
                        {"id": "1002", "type": 1, "nickname": "Ali"}
                    ]
                }
            });
            ws.send(WsMessage::Binary(
                compress_zlib_stream_frame(&mut comp, ready.to_string().as_bytes()).into(),
            ))
            .await
            .expect("send ready");

            // Send READY_SUPPLEMENTAL, CALL_CREATE (Group DM ring), VOICE_SERVER_UPDATE, STREAM_CREATE
            let dispatches = [
                serde_json::json!({
                    "op": 0, "s": 2, "t": "READY_SUPPLEMENTAL",
                    "d": {
                        "guilds": [],
                        "merged_presences": {
                            "friends": [{"user_id": "1002", "status": "online", "activities": [{"type": 4, "name": "Custom Status", "state": "Hacking BBS"}]}],
                            "guilds": []
                        }
                    }
                }),
                serde_json::json!({
                    "op": 0, "s": 3, "t": "CALL_CREATE",
                    "d": {
                        "channel_id": "4001",
                        "ringing": ["1001"],
                        "voice_states": [{"channel_id": "4001", "user_id": "1002", "session_id": "vs-call"}]
                    }
                }),
                serde_json::json!({
                    "op": 0, "s": 4, "t": "VOICE_SERVER_UPDATE",
                    "d": {
                        "token": "ephemeral-voice-cred",
                        "guild_id": null,
                        "channel_id": "4001",
                        "endpoint": "us-central.discord.gg:443"
                    }
                }),
                serde_json::json!({
                    "op": 0, "s": 5, "t": "STREAM_CREATE",
                    "d": {
                        "stream_key": "guild:2001:3002:1002",
                        "rtc_server_id": "rtc-99",
                        "region": "us-central",
                        "viewer_ids": ["1003"],
                        "paused": false
                    }
                }),
            ];

            for d in dispatches {
                ws.send(WsMessage::Binary(
                    compress_zlib_stream_frame(&mut comp, d.to_string().as_bytes()).into(),
                ))
                .await
                .expect("send dispatch");
            }

            // Receive client commands + first heartbeat, reply HEARTBEAT_ACK, then ask RECONNECT (op 7).
            let mut saw_hb = false;
            let mut saw_commands = 0usize;
            while let Some(Ok(frame)) = ws.next().await {
                if let Ok(txt) = frame.to_text()
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(txt)
                {
                    let op = val["op"].as_u64().unwrap_or(99);
                    if op == 1 {
                        saw_hb = true;
                        let ack = serde_json::json!({ "op": 11 });
                        let _ = ws
                            .send(WsMessage::Binary(
                                compress_zlib_stream_frame(&mut comp, ack.to_string().as_bytes())
                                    .into(),
                            ))
                            .await;
                    } else {
                        saw_commands += 1;
                    }
                    observed_clone.lock().await.push(val);
                    if saw_hb && saw_commands >= 5 {
                        let reconnect = serde_json::json!({ "op": 7 });
                        let _ = ws
                            .send(WsMessage::Binary(
                                compress_zlib_stream_frame(
                                    &mut comp,
                                    reconnect.to_string().as_bytes(),
                                )
                                .into(),
                            ))
                            .await;
                        break;
                    }
                }
            }
        }

        // Connection 2: expect RESUME (op 6) with session_id="sess-alpha-1" & seq=5!
        if let Ok((stream, _)) = listener.accept().await {
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept ws2");
            let mut comp = Compress::new(Compression::default(), true);
            let hello = serde_json::json!({ "op": 10, "d": { "heartbeat_interval": 30_000 } });
            ws.send(WsMessage::Binary(
                compress_zlib_stream_frame(&mut comp, hello.to_string().as_bytes()).into(),
            ))
            .await
            .expect("send hello2");

            if let Some(Ok(frame)) = ws.next().await
                && let Ok(txt) = frame.to_text()
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(txt)
                && val["op"] == 6
                && val["d"]["session_id"] == "sess-alpha-1"
                && val["d"]["seq"] == 5
            {
                resume_seen_clone.fetch_add(1, Ordering::SeqCst);
                let resumed = serde_json::json!({ "op": 0, "s": 6, "t": "RESUMED", "d": {} });
                let _ = ws
                    .send(WsMessage::Binary(
                        compress_zlib_stream_frame(&mut comp, resumed.to_string().as_bytes())
                            .into(),
                    ))
                    .await;
            }
            // Wait briefly for client shutdown close.
            let _ = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
        }
    });

    let (events_tx, mut events_rx) = broadcast::channel(128);
    let token = SecretToken::new("offline-test-session-cred").expect("token");
    let cfg = GatewayConfig {
        gateway_url: ws_url,
        capabilities: capabilities::DEFAULT_USER_CAPABILITIES,
        initial_reconnect_backoff: Duration::from_millis(20),
        max_reconnect_backoff: Duration::from_millis(100),
        heartbeat_jitter_override: Some(0.4),
    };

    let (gw, handle) = DiscordGateway::new(token, test_fingerprint(), cfg, events_tx);
    let mut sig_rx = handle.subscribe_signaling();
    let gw_task = tokio::spawn(gw.run());

    // Wait for READY event and verify Group DM recipients + read state + folders.
    let mut saw_ready = false;
    let mut saw_incoming_group_call = false;
    let mut saw_presence = false;
    let mut saw_active_stream = false;
    let mut saw_resumed = false;
    let mut saw_latency = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), events_rx.recv()).await {
            Ok(Ok(BackendEvent::Ready {
                current_user,
                guilds,
                guild_folders,
                dm_channels,
            })) => {
                assert_eq!(current_user.id, Snowflake(1001));
                assert_eq!(guilds.len(), 1);
                assert_eq!(guilds[0].name, "RetroBBS");
                assert!(guilds[0].unread);
                assert_eq!(guilds[0].mention_count, 2);
                assert_eq!(guild_folders.len(), 1);
                assert_eq!(guild_folders[0].id, Some(77));
                assert_eq!(dm_channels.len(), 1);
                assert_eq!(dm_channels[0].kind, ChannelKind::GroupDm);
                assert_eq!(dm_channels[0].recipients.len(), 2);
                assert_eq!(dm_channels[0].recipients[0].display_name(), "Alice");
                saw_ready = true;

                // Exercise outbound Op 4 (DM call + guild voice), Op 14, Op 18, Op 20, Op 22!
                handle
                    .update_voice_state(None, Some(Snowflake(4001)), false, false, false)
                    .expect("op4 dm");
                handle
                    .subscribe_guild_channel(Snowflake(2001), Snowflake(3001), vec![[0, 99]])
                    .expect("op14");
                handle
                    .stream_create(Some(Snowflake(2001)), Snowflake(3002), None)
                    .expect("op18");
                handle
                    .stream_watch("guild:2001:3002:1002")
                    .expect("op20");
                handle
                    .stream_set_paused("guild:2001:3002:1002", true)
                    .expect("op22");
            }
            Ok(Ok(BackendEvent::CallRingUpdated(CallRingState::IncomingRinging {
                channel_id,
                is_group_dm,
                ..
            }))) => {
                assert_eq!(channel_id, Snowflake(4001));
                assert!(is_group_dm);
                saw_incoming_group_call = true;
            }
            Ok(Ok(BackendEvent::PresenceUpdated(p))) => {
                assert_eq!(p.user_id, Snowflake(1002));
                assert_eq!(p.custom_status.as_deref(), Some("Hacking BBS"));
                saw_presence = true;
            }
            Ok(Ok(BackendEvent::ActiveStreamsUpdated { channel_id, streams })) => {
                assert_eq!(channel_id, Snowflake(3002));
                assert_eq!(streams.len(), 1);
                assert_eq!(streams[0].stream_key, "guild:2001:3002:1002");
                saw_active_stream = true;
            }
            Ok(Ok(BackendEvent::ConnectionChanged {
                state: ConnectionState::Ready,
                latency_ms,
                detail,
            })) => {
                if latency_ms.is_some() {
                    saw_latency = true;
                }
                if detail.as_deref() == Some("session resumed") {
                    saw_resumed = true;
                    break;
                }
            }
            _ => {}
        }
    }

    handle.shutdown().expect("shutdown");
    let gw_res = tokio::time::timeout(Duration::from_secs(3), gw_task)
        .await
        .expect("gateway exited")
        .expect("join ok");
    assert!(gw_res.is_ok());

    assert!(saw_ready, "READY must be emitted");
    assert!(saw_incoming_group_call, "Group DM CALL_CREATE must be emitted");
    assert!(saw_presence, "READY_SUPPLEMENTAL presence must be emitted");
    assert!(saw_active_stream, "STREAM_CREATE must emit ActiveStreamsUpdated");
    assert!(saw_latency, "HEARTBEAT_ACK must report latency_ms");
    assert!(saw_resumed, "Op 7 RECONNECT must complete RESUME -> RESUMED");
    assert_eq!(resume_seen.load(Ordering::SeqCst), 1);

    // Check signaling channel received VOICE_SERVER_UPDATE with redacted SecretToken.
    let mut saw_voice_server = false;
    while let Ok(sig) = sig_rx.try_recv() {
        if let GatewaySignalingEvent::VoiceServerUpdate {
            token,
            guild_id,
            channel_id,
            endpoint,
        } = sig
        {
            assert_eq!(guild_id, None);
            assert_eq!(channel_id, Some(Snowflake(4001)));
            assert_eq!(endpoint.as_deref(), Some("us-central.discord.gg:443"));
            assert_eq!(format!("{token:?}"), "SecretToken([REDACTED])");
            saw_voice_server = true;
        }
    }
    assert!(saw_voice_server);

    // Verify all outbound opcodes reached server.
    let ops: Vec<u64> = observed_ops
        .lock()
        .await
        .iter()
        .filter_map(|v| v["op"].as_u64())
        .collect();
    for expected_op in [1u64, 2, 4, 14, 18, 20, 22] {
        assert!(ops.contains(&expected_op), "missing outbound op {expected_op} in {ops:?}");
    }

    ws_server.abort();
}

#[test]
fn gateway_command_serialization_and_helpers() {
    // Op 4 with guild_id = None (DM/Group DM call)
    let op4 = serialize_gateway_command(&GatewayCommand::UpdateVoiceState {
        guild_id: None,
        channel_id: Some(Snowflake(4001)),
        self_mute: true,
        self_deaf: false,
        self_video: false,
    })
    .expect("op4")
    .expect("some");
    let v4: serde_json::Value = serde_json::from_str(&op4).expect("json");
    assert_eq!(v4["op"], opcodes::VOICE_STATE_UPDATE);
    assert!(v4["d"]["guild_id"].is_null());
    assert_eq!(v4["d"]["channel_id"], "4001");
    assert_eq!(v4["d"]["self_mute"], true);

    // Op 37 bulk guild subscriptions
    let mut subs = HashMap::new();
    let mut chans = HashMap::new();
    chans.insert(Snowflake(3001), vec![[0, 99]]);
    subs.insert(
        Snowflake(2001),
        GuildSubscriptionPayload {
            typing: Some(true),
            threads: Some(true),
            activities: Some(true),
            channels: chans,
            members: Vec::new(),
        },
    );
    let op37 = serialize_gateway_command(&GatewayCommand::GuildSubscriptionsBulk {
        subscriptions: subs,
    })
    .expect("op37")
    .expect("some");
    let v37: serde_json::Value = serde_json::from_str(&op37).expect("json");
    assert_eq!(v37["op"], opcodes::GUILD_SUBSCRIPTIONS_BULK);
    assert_eq!(v37["d"]["subscriptions"]["2001"]["channels"]["3001"][0], serde_json::json!([0, 99]));

    // Stream key parser & URL normalizer
    assert_eq!(
        parse_stream_key("guild:10:20:30"),
        Some((Some(Snowflake(10)), Snowflake(20), Snowflake(30)))
    );
    assert_eq!(
        parse_stream_key("call:20:30"),
        Some((None, Snowflake(20), Snowflake(30)))
    );
    assert_eq!(parse_stream_key("invalid"), None);
    assert_eq!(
        normalize_gateway_url("wss://gateway-us-east1-b.discord.gg"),
        "wss://gateway-us-east1-b.discord.gg/?v=10&encoding=json&compress=zlib-stream"
    );

    // Malformed gateway frame returns Err without panicking
    let mut state = GatewaySessionState::default();
    assert!(state.handle_raw_frame(b"not json").is_err());
}

// ============================================================================
// 4. QR Remote-Auth v2 End-to-End Local Handshake Test
// ============================================================================

#[tokio::test]
async fn remote_auth_qr_v2_handshake_end_to_end_local_wire() {
    let ws_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ra ws");
    let ws_addr = ws_listener.local_addr().expect("ra ws addr");
    let captured_pub_key: Arc<Mutex<Option<RsaPublicKey>>> = Arc::new(Mutex::new(None));
    let pub_key_for_http = Arc::clone(&captured_pub_key);

    let (http_base, http_server) = spawn_mock_http_server(move |req| {
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/api/v10/users/@me/remote-auth/login");
        assert_eq!(
            req.headers.get("referer").map(String::as_str),
            Some("https://discord.com/login")
        );
        assert_eq!(
            req.headers.get("x-fingerprint").map(String::as_str),
            Some("fp-qr-demo-99")
        );
        let body: serde_json::Value = serde_json::from_slice(&req.body).expect("ticket json");
        assert_eq!(body["ticket"], "ticket-approved-42");

        let pub_key = pub_key_for_http
            .try_lock()
            .ok()
            .and_then(|g| g.clone())
            .expect("public key captured");
        let mut rng = rsa::rand_core::OsRng;
        let encrypted_token = pub_key
            .encrypt(
                &mut rng,
                Oaep::new::<Sha256>(),
                b"offline-qr-decrypted-session",
            )
            .expect("encrypt token");
        MockHttpResponse::json(
            200,
            serde_json::json!({ "encrypted_token": BASE64_STANDARD.encode(encrypted_token) })
                .to_string(),
        )
    })
    .await;

    let pub_key_for_ws = Arc::clone(&captured_pub_key);
    let ws_server = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.expect("accept ra");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("upgrade ra");

        // 1. Server -> hello
        ws.send(WsMessage::Text(
            r#"{"op":"hello","heartbeat_interval":41250,"timeout_ms":120000}"#.into(),
        ))
        .await
        .expect("hello");

        // 2. Client -> init (SPKI public key)
        let init_txt = ws.next().await.expect("init").expect("ok");
        let init_val: serde_json::Value =
            serde_json::from_str(init_txt.to_text().expect("text")).expect("json");
        assert_eq!(init_val["op"], "init");
        let spki = BASE64_STANDARD
            .decode(init_val["encoded_public_key"].as_str().expect("pub key"))
            .expect("b64");
        let pub_key = RsaPublicKey::from_public_key_der(&spki).expect("spki der");
        *pub_key_for_ws.lock().await = Some(pub_key.clone());

        // 3. Server -> nonce_proof
        let raw_nonce = b"nonce-bytes-for-remote-auth-v2!!";
        let mut rng = rsa::rand_core::OsRng;
        let enc_nonce = pub_key
            .encrypt(&mut rng, Oaep::new::<Sha256>(), raw_nonce)
            .expect("encrypt nonce");
        ws.send(WsMessage::Text(
            serde_json::json!({
                "op": "nonce_proof",
                "encrypted_nonce": BASE64_STANDARD.encode(enc_nonce)
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send nonce_proof");

        // 4. Client -> nonce_proof (raw unpadded URL-safe base64)
        let proof_txt = ws.next().await.expect("proof").expect("ok");
        let proof_val: serde_json::Value =
            serde_json::from_str(proof_txt.to_text().expect("text")).expect("json");
        assert_eq!(proof_val["op"], "nonce_proof");
        assert_eq!(proof_val["nonce"], URL_SAFE_NO_PAD.encode(raw_nonce));

        // 5. Server -> pending_remote_init -> pending_ticket -> pending_login
        ws.send(WsMessage::Text(
            r#"{"op":"pending_remote_init","fingerprint":"fp-qr-demo-99"}"#.into(),
        ))
        .await
        .expect("pending_remote_init");

        let enc_user = pub_key
            .encrypt(&mut rng, Oaep::new::<Sha256>(), b"1001:0:hash42:qr_sysop")
            .expect("encrypt user");
        ws.send(WsMessage::Text(
            serde_json::json!({
                "op": "pending_ticket",
                "encrypted_user_payload": BASE64_STANDARD.encode(enc_user)
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("pending_ticket");

        ws.send(WsMessage::Text(
            r#"{"op":"pending_login","ticket":"ticket-approved-42"}"#.into(),
        ))
        .await
        .expect("pending_login");
    });

    let ra = RemoteAuthClient::new(test_fingerprint())
        .with_endpoints(format!("ws://{ws_addr}/?v=2"), format!("{http_base}/api/v10"));
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let token = ra.authenticate(ev_tx).await.expect("QR auth succeeded");
    assert_eq!(token.expose_secret(), "offline-qr-decrypted-session");

    let mut saw_qr = false;
    let mut saw_user = false;
    let mut saw_auth = false;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            RemoteAuthEvent::QrReady { qr_url, .. } => {
                assert_eq!(qr_url, "https://discord.com/ra/fp-qr-demo-99");
                saw_qr = true;
            }
            RemoteAuthEvent::PendingConfirmation(u) => {
                assert_eq!(u.username, "qr_sysop");
                assert_eq!(u.display_handle(), "qr_sysop");
                saw_user = true;
            }
            RemoteAuthEvent::Authenticated(_) => saw_auth = true,
            RemoteAuthEvent::Cancelled => panic!("unexpected cancel"),
        }
    }
    assert!(saw_qr && saw_user && saw_auth);

    ws_server.abort();
    http_server.abort();
}

// ============================================================================
// 5. GIF Providers (Discord Proxy, Klipy, Giphy, Direct .gif/.webp, Composite)
// ============================================================================

#[tokio::test]
async fn gif_providers_parse_discord_klipy_giphy_direct_and_redact_keys() {
    // Offline parser unit checks
    let discord_arr = br#"[{"id":"g1","title":"Matrix Rain","url":"https://tenor.com/view/1","src":"https://media.tenor.com/1.webm","gif_src":"https://media.tenor.com/1.gif","preview":"https://media.tenor.com/1-prev.png","width":498,"height":280}]"#;
    let dp = parse_discord_gifs_response(discord_arr).expect("discord array");
    assert_eq!(dp.items.len(), 1);
    assert_eq!(dp.items[0].provider, GifProviderKind::DiscordProxy);
    assert_eq!(dp.items[0].gif_url, "https://tenor.com/view/1");
    assert_eq!(dp.items[0].preview_url, "https://media.tenor.com/1-prev.png");

    // Preview selection must skip `.webm` / `.mp4` and fall through to decodable `.webp` / `.gif`.
    let klipy_discord_arr = br#"[{"id":"k1","title":"Klipy Wave","url":"https://klipy.com/gifs/wave","src":"https://static.klipy.com/wave.webm","gif_src":"https://static.klipy.com/wave.webp","preview":"https://static.klipy.com/wave-prev.webm","width":320,"height":240}]"#;
    let dp_klipy = parse_discord_gifs_response(klipy_discord_arr).expect("klipy discord array");
    assert_eq!(dp_klipy.items[0].gif_url, "https://klipy.com/gifs/wave");
    assert_eq!(
        dp_klipy.items[0].preview_url,
        "https://static.klipy.com/wave.webp"
    );

    let klipy_json = br#"{
        "result": true,
        "data": {
            "current_page": 1,
            "has_next": true,
            "data": [{
                "id": 987,
                "slug": "retro-modem",
                "title": "Retro Modem",
                "file": {
                    "hd": {"gif": {"url": "https://static.klipy.com/hd/modem.gif", "width": 480, "height": 320}},
                    "sm": {"webp": {"url": "https://static.klipy.com/sm/modem.webp", "width": 240, "height": 160}}
                }
            }]
        }
    }"#;
    let kp = parse_klipy_response(klipy_json).expect("klipy");
    assert_eq!(kp.items.len(), 1);
    assert_eq!(kp.items[0].provider, GifProviderKind::Klipy);
    assert_eq!(kp.items[0].gif_url, "https://static.klipy.com/hd/modem.gif");
    assert_eq!(kp.items[0].preview_url, "https://static.klipy.com/sm/modem.webp");
    assert_eq!(kp.next_cursor.as_deref(), Some("2"));

    let giphy_json = br#"{
        "data": [{
            "id": "abc123",
            "title": "CRT Glow",
            "images": {
                "original": {"url": "https://media.giphy.com/media/abc123/giphy.gif", "width": "400", "height": "300", "frames": "24"},
                "fixed_width_small": {"url": "https://media.giphy.com/media/abc123/100w.gif", "width": "100", "height": "75"}
            }
        }],
        "pagination": {"total_count": 50, "count": 1, "offset": 0}
    }"#;
    let gp = parse_giphy_response(giphy_json).expect("giphy");
    assert_eq!(gp.items.len(), 1);
    assert_eq!(gp.items[0].provider, GifProviderKind::Giphy);
    assert_eq!(gp.items[0].frame_count, Some(24));
    assert_eq!(gp.next_cursor.as_deref(), Some("1"));

    // Direct .gif & .webp URL resolution
    assert!(is_direct_gif_or_webp_url("https://example.org/assets/ansi-banner.gif?v=2"));
    assert!(is_direct_gif_or_webp_url("https://example.org/anim.WEBP"));
    assert!(!is_direct_gif_or_webp_url("retro cyberpunk"));
    let direct = resolve_direct_gif_url("https://example.org/assets/ansi-banner.gif?v=2")
        .expect("direct gif");
    assert_eq!(direct.title, "ansi banner");

    // Local HTTP wire test for Discord, Klipy, Giphy, Composite fallback, and HEAD probe
    let klipy_wire = klipy_json.to_vec();
    let giphy_wire = giphy_json.to_vec();
    let discord_wire = discord_arr.to_vec();

    let (base_url, server) = spawn_mock_http_server(move |req| match (req.method.as_str(), req.target.as_str()) {
        ("GET", t) if t.starts_with("/api/v10/gifs/search") => {
            assert!(
                t.contains("media_format=webp"),
                "expected media_format=webp in {t}"
            );
            assert!(t.contains("limit=20"), "expected limit clamped to 20 in {t}");
            assert!(
                !t.contains("provider=tenor"),
                "legacy provider=tenor must be omitted in {t}"
            );
            MockHttpResponse::bytes(200, "application/json", discord_wire.clone())
        }
        ("GET", t) if t.starts_with("/api/v10/gifs/trending-gifs") => {
            assert!(
                t.contains("media_format=webp"),
                "expected media_format=webp in {t}"
            );
            assert!(t.contains("limit=20"), "expected limit clamped to 20 in {t}");
            assert!(
                !t.contains("provider=tenor"),
                "legacy provider=tenor must be omitted in {t}"
            );
            MockHttpResponse::bytes(500, "application/json", b"{}".to_vec())
        }
        ("GET", t) if t.starts_with("/klipy/offline-test-klipy-key/gifs/") => {
            MockHttpResponse::bytes(200, "application/json", klipy_wire.clone())
        }
        ("GET", t) if t.starts_with("/giphy/search") && t.contains("api_key=offline-test-giphy-key") => {
            MockHttpResponse::bytes(200, "application/json", giphy_wire.clone())
        }
        ("HEAD", "/media/extensionless") => {
            MockHttpResponse::bytes(200, "image/webp", Vec::new())
        }
        _ => MockHttpResponse::json(401, r#"{"message":"unauthorized"}"#),
    })
    .await;

    let fp = test_fingerprint();
    let discord_gif = Arc::new(
        DiscordGifProvider::new(SecretToken::new("offline-test-session-cred").expect("tok"), &fp)
            .expect("discord gif")
            .with_api_base_url(format!("{base_url}/api/v10")),
    );
    let klipy_gif = Arc::new(
        KlipyGifProvider::new(SecretToken::new("offline-test-klipy-key").expect("k"), &fp)
            .expect("klipy")
            .with_base_url(format!("{base_url}/klipy")),
    );
    let giphy_gif = Arc::new(
        GiphyGifProvider::new(SecretToken::new("offline-test-giphy-key").expect("g"), &fp)
            .expect("giphy")
            .with_base_url(format!("{base_url}/giphy")),
    );

    // Verify Debug output redacts API keys
    assert!(!format!("{klipy_gif:?}").contains("offline-test-klipy-key"));
    assert!(!format!("{giphy_gif:?}").contains("offline-test-giphy-key"));

    // Primary Discord search
    let page = discord_gif
        .search(&GifSearchQuery {
            query: "matrix".to_owned(),
            limit: 10,
            cursor: None,
            trending: false,
            locale: Some("en-US".to_owned()),
        })
        .await
        .expect("discord search");
    assert_eq!(page.items[0].id, "g1");

    // Giphy direct search
    let gpage = giphy_gif
        .search(&GifSearchQuery {
            query: "crt".to_owned(),
            limit: 5,
            cursor: None,
            trending: false,
            locale: Some("en-US".to_owned()),
        })
        .await
        .expect("giphy search");
    assert_eq!(gpage.items[0].id, "abc123");

    // Composite fallback: Discord trending returns 500 -> falls back seamlessly to Klipy!
    let composite = CompositeGifProvider::new(discord_gif, vec![klipy_gif.clone(), giphy_gif]);
    let trending = composite.trending(10, None).await.expect("fallback to klipy");
    assert_eq!(trending.items[0].provider, GifProviderKind::Klipy);
    assert_eq!(trending.items[0].id, "987");

    // Extensionless direct URL HEAD probe
    let plain = fp.build_plain_browser_client().expect("plain");
    let probed = probe_direct_gif_url(&plain, &format!("{base_url}/media/extensionless"))
        .await
        .expect("head probe");
    assert!(probed.gif_url.ends_with("/media/extensionless"));

    // Security check: unreachable Klipy error MUST NOT leak the API key from the URL path!
    let bad_klipy = KlipyGifProvider::new(
        SecretToken::new("sentinel-ram-secret-never-log").expect("secret"),
        &fp,
    )
    .expect("bad klipy")
    .with_base_url("http://127.0.0.1:1");
    let err = bad_klipy
        .trending(5, None)
        .await
        .expect_err("unreachable port must fail");
    assert!(!err.to_string().contains("sentinel-ram-secret-never-log"));

    server.abort();
}

#[tokio::test]
async fn discord_gif_provider_handles_429_retry_after_and_hard_rate_limit() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_handler = Arc::clone(&attempts);

    let (base_url, server) = spawn_mock_http_server(move |req| {
        if req.target.starts_with("/api/v10/gifs/trending-gifs") {
            let n = attempts_handler.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return MockHttpResponse::json(
                    429,
                    r#"{"message":"You are being rate limited.","retry_after":0.02,"global":false}"#,
                );
            }
            return MockHttpResponse::json(
                200,
                r#"[{"id":"t1","title":"Trending OK","url":"https://klipy.com/gifs/t1","src":"https://static.klipy.com/t1.webp","gif_src":"https://static.klipy.com/t1.webp","preview":"https://static.klipy.com/t1.webp","width":200,"height":150}]"#,
            );
        }
        if req.target.starts_with("/api/v10/gifs/search") {
            return MockHttpResponse::json(
                429,
                r#"{"message":"Slow down","retry_after":30.0,"global":false}"#,
            );
        }
        MockHttpResponse::json(404, r#"{"message":"not found"}"#)
    })
    .await;

    let provider = DiscordGifProvider::new(
        SecretToken::new("offline-test-session-cred").expect("tok"),
        &test_fingerprint(),
    )
    .expect("gif provider")
    .with_api_base_url(format!("{base_url}/api/v10"));

    // Short 429 retry_after retries cleanly and returns trending results.
    let page = provider.trending(20, None).await.expect("retry succeeded");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].gif_url, "https://klipy.com/gifs/t1");

    // Long 429 retry_after surfaces CoreError::RateLimited immediately without hanging.
    let err = provider
        .search(&GifSearchQuery {
            query: "rate".to_owned(),
            limit: 20,
            cursor: None,
            trending: false,
            locale: Some("en-US".to_owned()),
        })
        .await
        .expect_err("long 429 must return RateLimited");
    assert!(
        matches!(err, CoreError::RateLimited { retry_after_ms } if retry_after_ms >= 29_000),
        "unexpected error: {err:?}"
    );

    server.abort();
}

// ============================================================================
// 6. REST Ack, Allowed Mentions, CDN Host Guard & Resize Preservation
// ============================================================================

#[tokio::test]
async fn rest_ack_message_allowed_mentions_and_cdn_host_guard() {
    // Pure host-guard checks
    assert!(is_discord_cdn_url(
        "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1"
    ));
    assert!(is_discord_cdn_url(
        "https://media.discordapp.net/attachments/1/2/a.png?width=320&height=240"
    ));
    assert!(is_discord_cdn_url(
        "https://images-ext-1.discordapp.net/external/h/https/example.invalid/a.gif"
    ));
    assert!(!is_discord_cdn_url("https://static.klipy.com/demo.webp"));
    assert!(!is_discord_cdn_url("https://tenor.com/view/demo"));
    assert!(!is_discord_cdn_url(
        "https://images-ext-evil.discordapp.net/a.png"
    ));

    // Resize query param preservation across CDN refresh
    let orig = "https://media.discordapp.net/attachments/1/2/anim.gif?ex=old&is=old&hm=old&width=320&height=240&format=webp&animated=true";
    let fresh = "https://media.discordapp.net/attachments/1/2/anim.gif?ex=new&is=new&hm=new";
    let merged = preserve_cdn_resize_params(orig, fresh);
    assert!(merged.contains("ex=new&is=new&hm=new"), "{merged}");
    assert!(merged.contains("width=320"), "{merged}");
    assert!(merged.contains("height=240"), "{merged}");
    assert!(merged.contains("format=webp"), "{merged}");
    assert!(merged.contains("animated=true"), "{merged}");

    let captured = Arc::new(Mutex::new(Vec::<CapturedHttpRequest>::new()));
    let captured_handler = Arc::clone(&captured);
    let refresh_hits = Arc::new(AtomicUsize::new(0));
    let refresh_hits_handler = Arc::clone(&refresh_hits);
    let base_holder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let base_for_handler = Arc::clone(&base_holder);

    let (base_url, server) = spawn_mock_http_server(move |req| {
        if let Ok(mut list) = captured_handler.try_lock() {
            list.push(req.clone());
        }
        let base = base_for_handler
            .try_lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        match (req.method.as_str(), req.target.as_str()) {
            ("POST", "/api/v10/channels/700/messages/900/ack") => {
                let body: serde_json::Value =
                    serde_json::from_slice(&req.body).expect("ack json");
                assert!(body.get("token").is_some_and(serde_json::Value::is_null));
                MockHttpResponse::json(200, r#"{"token":null}"#)
            }
            ("POST", "/api/v10/channels/700/messages") => MockHttpResponse::json(
                200,
                r#"{
                    "id":"901",
                    "channel_id":"700",
                    "author":{"id":"10","username":"sysop"},
                    "content":"silent reply",
                    "timestamp":"2026-11-26T00:00:00Z"
                }"#,
            ),
            ("GET", t) if t.starts_with("/cdn/expired.gif?ex=00000001") => {
                MockHttpResponse::bytes(404, "text/plain", b"expired".to_vec())
            }
            ("POST", "/api/v10/attachments/refresh-urls") => {
                refresh_hits_handler.fetch_add(1, Ordering::SeqCst);
                let parsed: serde_json::Value =
                    serde_json::from_slice(&req.body).expect("refresh json");
                let original = parsed["attachment_urls"][0].as_str().unwrap_or("");
                let refreshed = format!("{base}/cdn/fresh.gif?ex=70001234&hm=abcd");
                MockHttpResponse::json(
                    200,
                    serde_json::json!({
                        "refreshed_urls": [{
                            "original": original,
                            "refreshed": refreshed
                        }]
                    })
                    .to_string(),
                )
            }
            ("GET", t) if t.starts_with("/cdn/fresh.gif?") => {
                assert!(t.contains("ex=70001234"), "{t}");
                assert!(t.contains("width=256"), "{t}");
                assert!(t.contains("height=192"), "{t}");
                assert!(t.contains("format=webp"), "{t}");
                assert!(t.contains("animated=true"), "{t}");
                MockHttpResponse::bytes(200, "image/webp", b"RIFF....WEBP".to_vec())
            }
            ("GET", "/third-party/missing.webp") => {
                MockHttpResponse::bytes(404, "text/plain", b"not found".to_vec())
            }
            _ => MockHttpResponse::json(404, r#"{"message":"not found"}"#),
        }
    })
    .await;

    *base_holder.lock().await = base_url.clone();

    let client = DiscordRestClient::new(
        SecretToken::new("offline-test-session-cred").expect("tok"),
        test_fingerprint(),
    )
    .expect("client")
    .with_api_base_url(format!("{base_url}/api/v10"));
    let mut rx = client.subscribe_events();

    // 1. ack_message posts {"token": null} and emits MessageAcked.
    client
        .ack_message(Snowflake(700), Snowflake(900))
        .await
        .expect("ack");
    let ack_ev = rx.recv().await.expect("ack ev");
    assert!(matches!(
        ack_ev,
        BackendEvent::MessageAcked {
            channel_id: Snowflake(700),
            last_message_id: Snowflake(900),
            mention_count: 0,
        }
    ));

    // 2. send_message_with_mention(false) sets allowed_mentions.replied_user = false.
    let _ = client
        .send_message_with_mention(Snowflake(700), "silent reply", Some(Snowflake(900)), false)
        .await
        .expect("silent reply");

    // 3. Multipart upload with mention_replied_user = false also serializes allowed_mentions.
    let _ = client
        .send_message_with_attachments_and_mention(
            Snowflake(700),
            "silent pic",
            Some(Snowflake(900)),
            false,
            &[UploadAttachment {
                filename: "pic.png".to_owned(),
                content_type: Some("image/png".to_owned()),
                description: None,
                bytes: b"PNG".to_vec(),
            }],
        )
        .await
        .expect("multipart silent reply");

    // 4. Non-Discord host 404 MUST NOT call /attachments/refresh-urls.
    let third_party_url = format!("{base_url}/third-party/missing.webp");
    let err = client
        .fetch_attachment_bytes(&third_party_url)
        .await
        .expect_err("third-party 404 must fail directly");
    assert!(matches!(err, CoreError::Media(_)), "{err:?}");
    assert_eq!(refresh_hits.load(Ordering::SeqCst), 0);

    // 5. Expired CDN URL refreshes AND preserves width/height/format/animated params.
    let expired_resized =
        format!("{base_url}/cdn/expired.gif?ex=00000001&width=256&height=192&format=webp&animated=true");
    let (bytes, refreshed) = client
        .fetch_attachment_bytes(&expired_resized)
        .await
        .expect("resized refresh");
    assert_eq!(bytes, b"RIFF....WEBP");
    assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
    let refreshed = refreshed.expect("refreshed info");
    assert!(refreshed.refreshed_url.contains("width=256"));
    assert!(refreshed.refreshed_url.contains("format=webp"));
    assert!(refreshed.refreshed_url.contains("animated=true"));

    // Verify captured JSON + multipart bodies carried allowed_mentions.replied_user = false.
    let reqs = captured.lock().await;
    let msg_reqs: Vec<&CapturedHttpRequest> = reqs
        .iter()
        .filter(|r| r.method == "POST" && r.target == "/api/v10/channels/700/messages")
        .collect();
    assert_eq!(msg_reqs.len(), 2);
    let json_body: serde_json::Value =
        serde_json::from_slice(&msg_reqs[0].body).expect("json reply");
    assert_eq!(json_body["allowed_mentions"]["replied_user"], false);
    assert_eq!(
        json_body["allowed_mentions"]["parse"],
        serde_json::json!(["users", "roles", "everyone"])
    );
    let multipart_body = String::from_utf8_lossy(&msg_reqs[1].body);
    assert!(multipart_body.contains("\"replied_user\":false"));

    server.abort();
}

// ============================================================================
// 7. Gateway Roles, Mute Settings, Ack, Partial MESSAGE_UPDATE, Voice Roster & Scoped Calls
// ============================================================================

#[test]
fn gateway_roles_mute_settings_ack_partial_patch_voice_roster_and_scoped_calls() {
    let mut state = GatewaySessionState::default();

    // 1. READY with roles, members, versioned user_guild_settings (muted guild, muted channel,
    //    muted category), read_state (> comparison), and initial voice states.
    let ready_frame = serde_json::json!({
        "op": 0,
        "s": 1,
        "t": "READY",
        "d": {
            "session_id": "sess-roles-1",
            "user": {"id": "1001", "username": "sysop"},
            "users": [
                {"id": "1002", "username": "alice", "global_name": "Alice"},
                {"id": "1003", "username": "bob", "global_name": "Bob"}
            ],
            "read_state": {
                "version": 1,
                "entries": [
                    {"id": "3001", "last_message_id": "5000", "mention_count": 0},
                    {"id": "3003", "last_message_id": "5000", "mention_count": 0},
                    {"id": "3011", "last_message_id": "5000", "mention_count": 0},
                    {"id": "3012", "last_message_id": "6000", "mention_count": 0}
                ]
            },
            "user_guild_settings": {
                "version": 2,
                "entries": [
                    {
                        "guild_id": "2001",
                        "muted": false,
                        "suppress_everyone": true,
                        "suppress_roles": false,
                        "channel_overrides": [
                            {"channel_id": "3003", "muted": true},
                            {"channel_id": "3010", "muted": true}
                        ]
                    },
                    {
                        "guild_id": "2002",
                        "muted": true,
                        "suppress_everyone": true,
                        "suppress_roles": true,
                        "channel_overrides": []
                    }
                ]
            },
            "private_channels": [
                {"id": "4001", "type": 3, "name": "Ops Call", "recipient_ids": ["1002", "1003"]},
                {"id": "4002", "type": 1, "recipient_ids": ["1002"]}
            ],
            "guilds": [
                {
                    "id": "2001",
                    "properties": {"name": "RetroNet"},
                    "roles": [
                        {"id": "801", "name": "SysOp", "color": 3718648, "position": 20, "hoist": true},
                        {"id": "802", "name": "Member", "color": 5306995, "position": 5, "hoist": false}
                    ],
                    "members": [
                        {"user": {"id": "1002", "username": "alice"}, "nick": "Modem Wiz", "roles": ["801"]}
                    ],
                    "channels": [
                        {"id": "3001", "type": 0, "name": "general", "position": 0, "last_message_id": "5500"},
                        {"id": "3002", "type": 2, "name": "Lounge", "position": 1, "last_message_id": "9999"},
                        {"id": "3003", "type": 0, "name": "muted-logs", "position": 2, "last_message_id": "5500"},
                        {"id": "3010", "type": 4, "name": "MUTED CAT", "position": 3},
                        {"id": "3011", "type": 0, "name": "under-muted-cat", "parent_id": "3010", "position": 4, "last_message_id": "5500"},
                        {"id": "3012", "type": 0, "name": "deleted-tail", "position": 5, "last_message_id": "5900"}
                    ],
                    "voice_states": [
                        {"guild_id": "2001", "channel_id": "3002", "user_id": "1002", "session_id": "v1", "self_mute": false, "self_deaf": false}
                    ]
                },
                {
                    "id": "2002",
                    "properties": {"name": "Muted Guild"},
                    "channels": [
                        {"id": "3201", "type": 0, "name": "noise", "position": 0, "last_message_id": "7000"}
                    ]
                }
            ]
        }
    });
    let ready_out = state
        .handle_raw_frame(ready_frame.to_string().as_bytes())
        .expect("ready");

    // NEVER emit VoiceStatusUpdated from gateway!
    assert!(
        !ready_out
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::VoiceStatusUpdated(_)))
    );

    let ready_ev = ready_out
        .events
        .iter()
        .find_map(|e| match e {
            BackendEvent::Ready { guilds, .. } => Some(guilds),
            _ => None,
        })
        .expect("Ready event");
    assert_eq!(ready_ev[0].roles.len(), 2);
    assert_eq!(ready_ev[0].highest_role_color(&[Snowflake(801)]), Some(3718648));
    assert!(!ready_ev[0].muted);
    assert!(ready_ev[0].suppress_everyone);
    assert!(ready_ev[0].unread);
    assert!(ready_ev[1].muted);
    assert!(!ready_ev[1].unread, "muted guild must not mark unread");

    let g1_channels = ready_out
        .events
        .iter()
        .find_map(|e| match e {
            BackendEvent::ChannelsLoaded {
                guild_id: Some(Snowflake(2001)),
                channels,
            } => Some(channels),
            _ => None,
        })
        .expect("guild 2001 channels");
    let ch = |id: u64| g1_channels.iter().find(|c| c.id == Snowflake(id)).unwrap();
    assert_eq!(ch(3001).unread_count, 1);
    assert_eq!(ch(3001).last_ack_message_id, Some(Snowflake(5000)));
    assert_eq!(ch(3002).unread_count, 0, "voice channels must not be unread");
    assert!(ch(3003).muted);
    assert!(ch(3011).muted, "child of muted category must inherit muted");
    assert_eq!(
        ch(3012).unread_count,
        0,
        "last_message_id < ack_id (deleted tail) must not mark unread"
    );

    // READY must emit VoiceRosterUpdated, GuildSettingsUpdated, and GuildMembersUpdated.
    assert!(ready_out.events.iter().any(|e| matches!(
        e,
        BackendEvent::VoiceRosterUpdated {
            guild_id: Some(Snowflake(2001)),
            channel_id: Snowflake(3002),
            participants,
        } if participants.len() == 1 && participants[0].user_id == Snowflake(1002)
    )));
    assert!(ready_out.events.iter().any(|e| matches!(
        e,
        BackendEvent::GuildSettingsUpdated {
            guild_id: Some(Snowflake(2001)),
            suppress_everyone: true,
            ..
        }
    )));
    assert!(ready_out.events.iter().any(|e| matches!(
        e,
        BackendEvent::GuildMembersUpdated {
            guild_id: Snowflake(2001),
            members,
        } if members.len() == 1 && members[0].nick.as_deref() == Some("Modem Wiz")
    )));

    // 2. READY_SUPPLEMENTAL emits VoiceRosterUpdated (never VoiceStatusUpdated).
    let supp_frame = serde_json::json!({
        "op": 0, "s": 2, "t": "READY_SUPPLEMENTAL",
        "d": {
            "guilds": [{
                "id": "2001",
                "voice_states": [
                    {"guild_id": "2001", "channel_id": "3002", "user_id": "1003", "session_id": "v2", "self_mute": true}
                ]
            }]
        }
    });
    let supp_out = state
        .handle_raw_frame(supp_frame.to_string().as_bytes())
        .expect("supp");
    assert!(
        !supp_out
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::VoiceStatusUpdated(_)))
    );
    assert!(supp_out.events.iter().any(|e| matches!(
        e,
        BackendEvent::VoiceRosterUpdated {
            channel_id: Snowflake(3002),
            participants,
            ..
        } if participants.len() == 2
    )));

    // 3. VOICE_STATE_UPDATE move & leave update BOTH old and new channels via VoiceRosterUpdated.
    let move_frame = serde_json::json!({
        "op": 0, "s": 3, "t": "VOICE_STATE_UPDATE",
        "d": {"guild_id": "2001", "channel_id": "3099", "user_id": "1003", "session_id": "v2", "self_mute": false}
    });
    let move_out = state
        .handle_raw_frame(move_frame.to_string().as_bytes())
        .expect("move");
    assert!(
        !move_out
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::VoiceStatusUpdated(_)))
    );
    assert!(move_out.events.iter().any(|e| matches!(
        e,
        BackendEvent::VoiceRosterUpdated {
            channel_id: Snowflake(3002),
            participants,
            ..
        } if participants.len() == 1
    )));
    assert!(move_out.events.iter().any(|e| matches!(
        e,
        BackendEvent::VoiceRosterUpdated {
            channel_id: Snowflake(3099),
            participants,
            ..
        } if participants.len() == 1 && participants[0].user_id == Snowflake(1003)
    )));

    // 4. MESSAGE_CREATE with member nick/roles + mention_roles, then partial MESSAGE_UPDATE embed unfurl.
    let msg_create = serde_json::json!({
        "op": 0, "s": 4, "t": "MESSAGE_CREATE",
        "d": {
            "id": "7001", "channel_id": "3001", "guild_id": "2001",
            "author": {"id": "1002", "username": "alice"},
            "member": {"nick": "Modem Wiz", "roles": ["801"]},
            "mention_roles": ["802"],
            "content": "https://klipy.com/gifs/demo",
            "timestamp": "2026-11-26T00:00:00Z"
        }
    });
    let mc_out = state
        .handle_raw_frame(msg_create.to_string().as_bytes())
        .expect("msg create");
    let mut cached_msg = mc_out
        .events
        .into_iter()
        .find_map(|e| match e {
            BackendEvent::MessageCreated(m) => Some(m),
            _ => None,
        })
        .expect("MessageCreated");
    assert_eq!(cached_msg.author_nick.as_deref(), Some("Modem Wiz"));
    assert_eq!(cached_msg.author_role_ids, vec![Snowflake(801)]);
    assert_eq!(cached_msg.mention_roles, vec![Snowflake(802)]);

    let partial_unfurl = serde_json::json!({
        "op": 0, "s": 5, "t": "MESSAGE_UPDATE",
        "d": {
            "id": "7001", "channel_id": "3001",
            "embeds": [{"type": "gifv", "title": "Demo GIF", "thumbnail": {"url": "https://static.klipy.com/demo.webp"}}]
        }
    });
    let patch_out = state
        .handle_raw_frame(partial_unfurl.to_string().as_bytes())
        .expect("partial update");
    assert!(
        !patch_out
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::MessageUpdated(_)))
    );
    let patch = patch_out
        .events
        .into_iter()
        .find_map(|e| match e {
            BackendEvent::MessagePatched(p) => Some(p),
            _ => None,
        })
        .expect("MessagePatched");
    cached_msg.apply_patch(patch);
    assert_eq!(cached_msg.author.username, "alice");
    assert_eq!(cached_msg.content, "https://klipy.com/gifs/demo");
    assert_eq!(cached_msg.embeds.len(), 1);

    // 5. MESSAGE_ACK, USER_GUILD_SETTINGS_UPDATE, GUILD_ROLE_*, GUILD_MEMBER_UPDATE, GUILD_MEMBERS_CHUNK.
    let ack_out = state
        .handle_raw_frame(
            br#"{"op":0,"s":6,"t":"MESSAGE_ACK","d":{"channel_id":"3001","message_id":"7001","mention_count":0}}"#,
        )
        .expect("ack");
    assert!(matches!(
        ack_out.events.as_slice(),
        [BackendEvent::MessageAcked {
            channel_id: Snowflake(3001),
            last_message_id: Snowflake(7001),
            mention_count: 0,
        }]
    ));

    let settings_out = state
        .handle_raw_frame(
            br#"{"op":0,"s":7,"t":"USER_GUILD_SETTINGS_UPDATE","d":{"guild_id":"2001","muted":true,"suppress_everyone":true,"suppress_roles":true,"channel_overrides":[{"channel_id":"3001","muted":true}]}}"#,
        )
        .expect("settings update");
    assert!(matches!(
        settings_out.events.as_slice(),
        [BackendEvent::GuildSettingsUpdated {
            guild_id: Some(Snowflake(2001)),
            muted: true,
            suppress_everyone: true,
            suppress_roles: true,
            ..
        }]
    ));

    let role_create = state
        .handle_raw_frame(
            br#"{"op":0,"s":8,"t":"GUILD_ROLE_CREATE","d":{"guild_id":"2001","role":{"id":"803","name":"VIP","color":16744192,"position":30,"hoist":true}}}"#,
        )
        .expect("role create");
    assert!(matches!(
        role_create.events.as_slice(),
        [BackendEvent::GuildRolesUpdated { guild_id: Snowflake(2001), roles }] if roles.len() == 3
    ));
    let role_del = state
        .handle_raw_frame(
            br#"{"op":0,"s":9,"t":"GUILD_ROLE_DELETE","d":{"guild_id":"2001","role_id":"803"}}"#,
        )
        .expect("role delete");
    assert!(matches!(
        role_del.events.as_slice(),
        [BackendEvent::GuildRolesUpdated { guild_id: Snowflake(2001), roles }] if roles.len() == 2
    ));

    let member_upd = state
        .handle_raw_frame(
            br#"{"op":0,"s":10,"t":"GUILD_MEMBER_UPDATE","d":{"guild_id":"2001","user":{"id":"1003","username":"bob"},"nick":"CRT Bob","roles":["802"]}}"#,
        )
        .expect("member update");
    assert!(matches!(
        member_upd.events.as_slice(),
        [BackendEvent::GuildMembersUpdated { guild_id: Snowflake(2001), members }]
            if members.len() == 1 && members[0].nick.as_deref() == Some("CRT Bob")
    ));

    let member_chunk = state
        .handle_raw_frame(
            br#"{"op":0,"s":11,"t":"GUILD_MEMBERS_CHUNK","d":{"guild_id":"2001","members":[{"user":{"id":"1004","username":"carol"},"nick":"ANSI","roles":["801","802"]}]}}"#,
        )
        .expect("member chunk");
    assert!(matches!(
        member_chunk.events.as_slice(),
        [BackendEvent::GuildMembersUpdated { guild_id: Snowflake(2001), members }]
            if members.len() == 1 && members[0].user_id == Snowflake(1004)
    ));

    // 6. Op 8 REQUEST_GUILD_MEMBERS serialization (query vs user_ids mutually exclusive).
    let op8_query = serialize_gateway_command(&GatewayCommand::RequestGuildMembers {
        guild_id: Snowflake(2001),
        query: Some("ali".to_owned()),
        user_ids: Vec::new(),
        limit: 25,
    })
    .expect("op8 query")
    .expect("some");
    let v8q: serde_json::Value = serde_json::from_str(&op8_query).expect("json");
    assert_eq!(v8q["op"], opcodes::REQUEST_GUILD_MEMBERS);
    assert_eq!(v8q["d"]["query"], "ali");
    assert!(v8q["d"].get("user_ids").is_none());

    let op8_ids = serialize_gateway_command(&GatewayCommand::RequestGuildMembers {
        guild_id: Snowflake(2001),
        query: None,
        user_ids: vec![Snowflake(1002)],
        limit: 25,
    })
    .expect("op8 ids")
    .expect("some");
    let v8i: serde_json::Value = serde_json::from_str(&op8_ids).expect("json");
    assert_eq!(v8i["op"], opcodes::REQUEST_GUILD_MEMBERS);
    assert_eq!(v8i["d"]["user_ids"], serde_json::json!(["1002"]));
    assert!(v8i["d"].get("query").is_none());

    // 7. Channel-scoped CALL_CREATE / CALL_UPDATE / CALL_DELETE signaling & non-clobbering.
    let call_create = state
        .handle_raw_frame(
            br#"{"op":0,"s":12,"t":"CALL_CREATE","d":{"channel_id":"4001","ringing":["1001"],"voice_states":[{"channel_id":"4001","user_id":"1002","session_id":"vs-call"}]}}"#,
        )
        .expect("call create");
    assert!(call_create.signaling.iter().any(|s| matches!(
        s,
        GatewaySignalingEvent::CallCreate {
            channel_id: Snowflake(4001),
            is_group_dm: true,
            ..
        }
    )));
    assert!(call_create.events.iter().any(|e| matches!(
        e,
        BackendEvent::CallRingUpdated(CallRingState::IncomingRinging {
            channel_id: Snowflake(4001),
            ..
        })
    )));

    // Unrelated CALL_DELETE for channel 4002 must NOT clear active call on 4001!
    let unrelated_del = state
        .handle_raw_frame(br#"{"op":0,"s":13,"t":"CALL_DELETE","d":{"channel_id":"4002"}}"#)
        .expect("unrelated call delete");
    assert!(unrelated_del.signaling.iter().any(|s| matches!(
        s,
        GatewaySignalingEvent::CallDelete {
            channel_id: Snowflake(4002),
        }
    )));
    assert!(
        !unrelated_del
            .events
            .iter()
            .any(|e| matches!(e, BackendEvent::CallRingUpdated(CallRingState::Idle))),
        "CALL_DELETE for unrelated channel must not clear active call ring state"
    );

    // Matching CALL_DELETE for 4001 emits CallDelete signaling AND clears CallRingState::Idle.
    let matching_del = state
        .handle_raw_frame(br#"{"op":0,"s":14,"t":"CALL_DELETE","d":{"channel_id":"4001"}}"#)
        .expect("matching call delete");
    assert!(matching_del.signaling.iter().any(|s| matches!(
        s,
        GatewaySignalingEvent::CallDelete {
            channel_id: Snowflake(4001),
        }
    )));
    assert!(matching_del
        .events
        .iter()
        .any(|e| matches!(e, BackendEvent::CallRingUpdated(CallRingState::Idle))));
}
