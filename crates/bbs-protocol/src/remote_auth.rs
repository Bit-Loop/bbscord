//! Discord QR Remote-Auth v2 WebSocket handshake (`wss://remote-auth-gateway.discord.gg/?v=2`).
//!
//! Wire protocol cross-referenced against `discordo/internal/ui/login/qr/{model,msg}.go`:
//! 1. Generate an ephemeral 2048-bit RSA keypair.
//! 2. Connect to Remote Auth Gateway v2, receive `hello`, send `init` with Base64 SPKI DER public key.
//! 3. Receive `nonce_proof`, decrypt `encrypted_nonce` via RSA-OAEP(SHA-256), reply with unpadded
//!    URL-safe Base64 of the decrypted nonce bytes.
//! 4. Receive `pending_remote_init` with `fingerprint` -> emit QR login URL `https://discord.com/ra/<fingerprint>`.
//! 5. Receive `pending_ticket` -> decrypt `encrypted_user_payload` (`id:discriminator:avatar:username`).
//! 6. Receive `pending_login` with `ticket` -> `POST /api/v10/users/@me/remote-auth/login` with
//!    `X-Fingerprint` and `Referer: https://discord.com/login`, RSA-OAEP(SHA-256) decrypt
//!    `encrypted_token`, and return a zeroizing [`SecretToken`].

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use bbs_core::{CoreError, CoreResult, SecretToken};
use boring2::rsa::Rsa as BoringRsa;
use futures::{SinkExt, StreamExt};
use http::header;
use rsa::pkcs8::EncodePublicKey;
use rsa::{BigUint, Oaep, RsaPrivateKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::fingerprint::{DISCORD_ORIGIN, DiscordFingerprint};

/// Production Discord Remote Auth v2 WebSocket endpoint.
pub const REMOTE_AUTH_GATEWAY_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
/// Base URL prefix encoded into the mobile-scannable QR code.
pub const REMOTE_AUTH_QR_PREFIX: &str = "https://discord.com/ra/";
/// Discord REST login page referer used during ticket exchange.
const REMOTE_AUTH_LOGIN_REFERER: &str = "https://discord.com/login";
/// Default Discord API v10 base URL for ticket exchange.
const DEFAULT_API_BASE_URL: &str = "https://discord.com/api/v10";

/// Ephemeral 2048-bit RSA keypair used for one QR Remote-Auth handshake.
pub struct RemoteAuthKeypair {
    private_key: RsaPrivateKey,
    spki_der_base64: String,
}

impl std::fmt::Debug for RemoteAuthKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteAuthKeypair")
            .field("bits", &2048)
            .field("private_key", &"[REDACTED]")
            .finish()
    }
}

impl RemoteAuthKeypair {
    /// Generate a fresh 2048-bit RSA keypair using BoringSSL for fast keygen and `rsa` for OAEP-SHA256.
    pub fn generate() -> CoreResult<Self> {
        let boring_key = BoringRsa::generate(2048)
            .map_err(|err| CoreError::Protocol(format!("RSA-2048 key generation failed: {err}")))?;

        let n_bytes = boring_key.n().to_vec();
        let e_bytes = boring_key.e().to_vec();
        let d_bytes = Zeroizing::new(boring_key.d().to_vec());
        let p_bytes = Zeroizing::new(
            boring_key
                .p()
                .ok_or_else(|| CoreError::Protocol("RSA key missing prime p".to_owned()))?
                .to_vec(),
        );
        let q_bytes = Zeroizing::new(
            boring_key
                .q()
                .ok_or_else(|| CoreError::Protocol("RSA key missing prime q".to_owned()))?
                .to_vec(),
        );

        let private_key = RsaPrivateKey::from_components(
            BigUint::from_bytes_be(&n_bytes),
            BigUint::from_bytes_be(&e_bytes),
            BigUint::from_bytes_be(&d_bytes),
            vec![
                BigUint::from_bytes_be(&p_bytes),
                BigUint::from_bytes_be(&q_bytes),
            ],
        )
        .map_err(|err| CoreError::Protocol(format!("construct RSA private key: {err}")))?;

        let spki_der = private_key
            .to_public_key()
            .to_public_key_der()
            .map_err(|err| CoreError::Protocol(format!("encode RSA SPKI DER: {err}")))?;
        let spki_der_base64 = BASE64_STANDARD.encode(spki_der.as_bytes());

        Ok(Self {
            private_key,
            spki_der_base64,
        })
    }

    /// Base64-encoded SubjectPublicKeyInfo (DER) sent in the Remote-Auth `init` message.
    #[must_use]
    pub fn encoded_public_key(&self) -> &str {
        &self.spki_der_base64
    }

    /// Decrypt an RSA-OAEP(SHA-256) Base64 ciphertext into a zeroizing byte vector.
    pub fn decrypt_oaep_sha256_b64(&self, ciphertext_b64: &str) -> CoreResult<Zeroizing<Vec<u8>>> {
        let ciphertext = BASE64_STANDARD
            .decode(ciphertext_b64.trim())
            .map_err(|err| CoreError::Protocol(format!("decode remote-auth base64: {err}")))?;
        let plaintext = self
            .private_key
            .decrypt(Oaep::new::<Sha256>(), &ciphertext)
            .map_err(|err| CoreError::Protocol(format!("RSA-OAEP-SHA256 decrypt failed: {err}")))?;
        Ok(Zeroizing::new(plaintext))
    }

    /// Decrypt `encrypted_nonce` and encode the raw nonce as unpadded URL-safe Base64 (Remote Auth v2).
    pub fn solve_nonce_proof(&self, encrypted_nonce_b64: &str) -> CoreResult<String> {
        let decrypted = self.decrypt_oaep_sha256_b64(encrypted_nonce_b64)?;
        Ok(URL_SAFE_NO_PAD.encode(decrypted.as_slice()))
    }

    /// Decrypt `encrypted_user_payload` (`"<user_id>:<discriminator>:<avatar_hash>:<username>"`).
    pub fn decrypt_user_payload(
        &self,
        encrypted_payload_b64: &str,
    ) -> CoreResult<RemoteAuthPendingUser> {
        let decrypted = self.decrypt_oaep_sha256_b64(encrypted_payload_b64)?;
        let text = Zeroizing::new(String::from_utf8(decrypted.to_vec()).map_err(|err| {
            CoreError::Protocol(format!("remote-auth user payload not UTF-8: {err}"))
        })?);
        parse_decrypted_user_payload(&text)
    }

    /// Decrypt `encrypted_token` from ticket exchange directly into a zeroizing [`SecretToken`].
    pub fn decrypt_token(&self, encrypted_token_b64: &str) -> CoreResult<SecretToken> {
        let decrypted = self.decrypt_oaep_sha256_b64(encrypted_token_b64)?;
        let token_str = Zeroizing::new(String::from_utf8(decrypted.to_vec()).map_err(|err| {
            CoreError::Protocol(format!("remote-auth token not UTF-8: {err}"))
        })?);
        SecretToken::new(token_str.as_str())
    }
}

/// Decrypted preview identity emitted once the mobile Discord app scans the QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAuthPendingUser {
    /// Scanned user's Snowflake string.
    pub user_id: String,
    /// Legacy discriminator (`"0"` for modern handles).
    pub discriminator: String,
    /// Optional avatar hash.
    pub avatar_hash: Option<String>,
    /// Account username.
    pub username: String,
}

impl RemoteAuthPendingUser {
    /// Human-readable display label (`username` or `username#discriminator`).
    #[must_use]
    pub fn display_handle(&self) -> String {
        if self.discriminator.is_empty() || self.discriminator == "0" {
            self.username.clone()
        } else {
            format!("{}#{}", self.username, self.discriminator)
        }
    }
}

/// Parse Discord Remote-Auth decrypted user payload (`id:discriminator:avatar:username`).
pub fn parse_decrypted_user_payload(raw: &str) -> CoreResult<RemoteAuthPendingUser> {
    let mut parts = raw.splitn(4, ':');
    let user_id = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Protocol("remote-auth user payload missing user_id".to_owned()))?;
    let discriminator = parts.next().ok_or_else(|| {
        CoreError::Protocol("remote-auth user payload missing discriminator".to_owned())
    })?;
    let avatar = parts.next().ok_or_else(|| {
        CoreError::Protocol("remote-auth user payload missing avatar".to_owned())
    })?;
    let username = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CoreError::Protocol("remote-auth user payload missing username".to_owned()))?;

    Ok(RemoteAuthPendingUser {
        user_id: user_id.to_owned(),
        discriminator: discriminator.to_owned(),
        avatar_hash: if avatar.is_empty() || avatar == "0" {
            None
        } else {
            Some(avatar.to_owned())
        },
        username: username.to_owned(),
    })
}

/// Progress events emitted during a QR Remote-Auth login handshake.
#[derive(Debug, Clone)]
pub enum RemoteAuthEvent {
    /// QR fingerprint is ready; display `qr_url` (`https://discord.com/ra/<fingerprint>`) as a QR code.
    QrReady {
        /// Full URL to encode into the QR image.
        qr_url: String,
        /// Raw fingerprint string.
        fingerprint: String,
        /// Server-advertised handshake timeout in milliseconds.
        timeout_ms: u64,
    },
    /// Mobile client scanned the QR code and is awaiting user confirmation.
    PendingConfirmation(RemoteAuthPendingUser),
    /// User cancelled the login prompt on mobile.
    Cancelled,
    /// Login succeeded and produced a zeroizing [`SecretToken`].
    Authenticated(SecretToken),
}

/// Inbound frame sent by Discord Remote-Auth Gateway v2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAuthServerFrame {
    /// Initial handshake parameters.
    Hello {
        /// Heartbeat period in milliseconds.
        heartbeat_interval_ms: u64,
        /// Overall QR session timeout in milliseconds.
        timeout_ms: u64,
    },
    /// RSA-OAEP nonce challenge.
    NonceProof {
        /// Standard Base64 encrypted nonce.
        encrypted_nonce: String,
    },
    /// QR fingerprint ready to display.
    PendingRemoteInit {
        /// Fingerprint appended to `https://discord.com/ra/`.
        fingerprint: String,
    },
    /// Mobile scanned QR and encrypted user preview payload.
    PendingTicket {
        /// Standard Base64 encrypted user payload.
        encrypted_user_payload: String,
    },
    /// Mobile approved login; exchange `ticket` over REST for the encrypted token.
    PendingLogin {
        /// One-time remote-auth ticket.
        ticket: String,
    },
    /// Mobile cancelled login.
    Cancel,
    /// Heartbeat acknowledgement or unknown benign op.
    Ignored,
}

#[derive(Debug, Deserialize)]
struct WireRemoteAuthFrame {
    op: String,
    #[serde(default)]
    heartbeat_interval: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    encrypted_nonce: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    encrypted_user_payload: Option<String>,
    #[serde(default)]
    ticket: Option<String>,
}

/// Decode a JSON text frame from Discord Remote Auth Gateway v2.
pub fn decode_remote_auth_frame(text: &str) -> CoreResult<RemoteAuthServerFrame> {
    let wire: WireRemoteAuthFrame = serde_json::from_str(text)
        .map_err(|err| CoreError::Protocol(format!("parse remote-auth frame: {err}")))?;

    match wire.op.as_str() {
        "hello" => Ok(RemoteAuthServerFrame::Hello {
            heartbeat_interval_ms: wire.heartbeat_interval.unwrap_or(41_250),
            timeout_ms: wire.timeout_ms.unwrap_or(120_000),
        }),
        "nonce_proof" => {
            let encrypted_nonce = wire.encrypted_nonce.ok_or_else(|| {
                CoreError::Protocol("remote-auth nonce_proof missing encrypted_nonce".to_owned())
            })?;
            Ok(RemoteAuthServerFrame::NonceProof { encrypted_nonce })
        }
        "pending_remote_init" => {
            let fingerprint = wire.fingerprint.ok_or_else(|| {
                CoreError::Protocol("remote-auth pending_remote_init missing fingerprint".to_owned())
            })?;
            Ok(RemoteAuthServerFrame::PendingRemoteInit { fingerprint })
        }
        "pending_ticket" => {
            let encrypted_user_payload = wire.encrypted_user_payload.ok_or_else(|| {
                CoreError::Protocol(
                    "remote-auth pending_ticket missing encrypted_user_payload".to_owned(),
                )
            })?;
            Ok(RemoteAuthServerFrame::PendingTicket {
                encrypted_user_payload,
            })
        }
        "pending_login" => {
            let ticket = wire.ticket.ok_or_else(|| {
                CoreError::Protocol("remote-auth pending_login missing ticket".to_owned())
            })?;
            Ok(RemoteAuthServerFrame::PendingLogin { ticket })
        }
        "cancel" => Ok(RemoteAuthServerFrame::Cancel),
        _ => Ok(RemoteAuthServerFrame::Ignored),
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum OutboundRemoteAuthFrame<'a> {
    Init { encoded_public_key: &'a str },
    NonceProof { nonce: &'a str },
    Heartbeat,
}

#[derive(Debug, Serialize)]
struct TicketExchangeRequest<'a> {
    ticket: &'a str,
}

#[derive(Debug, Deserialize)]
struct TicketExchangeResponse {
    encrypted_token: String,
}

/// Configuration for running a QR Remote-Auth v2 login session (customizable endpoints for tests).
#[derive(Debug, Clone)]
pub struct RemoteAuthClient {
    fingerprint: DiscordFingerprint,
    gateway_url: String,
    api_base_url: String,
}

impl RemoteAuthClient {
    /// Create a Remote-Auth client targeting production Discord endpoints.
    #[must_use]
    pub fn new(fingerprint: DiscordFingerprint) -> Self {
        Self {
            fingerprint,
            gateway_url: REMOTE_AUTH_GATEWAY_URL.to_owned(),
            api_base_url: DEFAULT_API_BASE_URL.to_owned(),
        }
    }

    /// Override WebSocket gateway and REST API base URLs (intended for offline wire tests).
    #[must_use]
    pub fn with_endpoints(
        mut self,
        gateway_url: impl Into<String>,
        api_base_url: impl Into<String>,
    ) -> Self {
        self.gateway_url = gateway_url.into();
        self.api_base_url = api_base_url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Exchange a Remote-Auth login `ticket` over REST and decrypt the returned token into RAM.
    pub async fn exchange_ticket(
        &self,
        keypair: &RemoteAuthKeypair,
        fingerprint_header: &str,
        ticket: &str,
    ) -> CoreResult<SecretToken> {
        let rest_client = self.fingerprint.build_rest_client()?;
        let endpoint = format!("{}/users/@me/remote-auth/login", self.api_base_url);

        let mut req = rest_client
            .post(&endpoint)
            .header(header::REFERER, REMOTE_AUTH_LOGIN_REFERER)
            .json(&TicketExchangeRequest { ticket });

        if !fingerprint_header.is_empty() {
            req = req.header("x-fingerprint", fingerprint_header);
        }

        let resp = req
            .send()
            .await
            .map_err(|err| CoreError::Network(format!("remote-auth ticket exchange: {err}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(CoreError::Protocol(format!(
                "remote-auth ticket exchange failed with HTTP {}",
                status.as_u16()
            )));
        }

        let payload: TicketExchangeResponse = resp.json().await.map_err(|err| {
            CoreError::Protocol(format!("decode remote-auth ticket response: {err}"))
        })?;
        keypair.decrypt_token(&payload.encrypted_token)
    }

    /// Execute the full Remote-Auth v2 handshake, emitting [`RemoteAuthEvent`] milestones
    /// on `event_tx` and returning the zeroizing [`SecretToken`] upon approval.
    pub async fn authenticate(
        &self,
        event_tx: mpsc::UnboundedSender<RemoteAuthEvent>,
    ) -> CoreResult<SecretToken> {
        let keypair = tokio::task::spawn_blocking(RemoteAuthKeypair::generate)
            .await
            .map_err(|err| CoreError::Internal(format!("remote-auth keygen task failed: {err}")))??;
        let ws_client = self.fingerprint.build_websocket_client()?;

        let ws_resp = ws_client
            .websocket(&self.gateway_url)
            .header(header::ORIGIN, DISCORD_ORIGIN)
            .header(header::USER_AGENT, self.fingerprint.user_agent())
            .send()
            .await
            .map_err(|err| CoreError::Network(format!("connect remote-auth gateway: {err}")))?;

        let ws = ws_resp
            .into_websocket()
            .await
            .map_err(|err| CoreError::Network(format!("upgrade remote-auth websocket: {err}")))?;

        let (mut ws_sink, mut ws_stream) = ws.split();
        let mut timeout_ms = 120_000u64;
        let mut deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut fingerprint_str = String::new();
        let mut heartbeat_interval: Option<tokio::time::Interval> = None;

        loop {
            let next_msg = if let Some(hb) = heartbeat_interval.as_mut() {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        let _ = ws_sink.close().await;
                        return Err(CoreError::Network("remote-auth QR session timed out".to_owned()));
                    }
                    _ = hb.tick() => {
                        let hb_json = serde_json::to_string(&OutboundRemoteAuthFrame::Heartbeat)
                            .map_err(|err| CoreError::Protocol(format!("serialize remote-auth heartbeat: {err}")))?;
                        ws_sink
                            .send(wreq::Message::text(hb_json))
                            .await
                            .map_err(|err| CoreError::Network(format!("send remote-auth heartbeat: {err}")))?;
                        continue;
                    }
                    msg = ws_stream.next() => msg,
                }
            } else {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        let _ = ws_sink.close().await;
                        return Err(CoreError::Network("remote-auth handshake timed out".to_owned()));
                    }
                    msg = ws_stream.next() => msg,
                }
            };

            let Some(frame_res) = next_msg else {
                return Err(CoreError::Network(
                    "remote-auth gateway closed before login completed".to_owned(),
                ));
            };

            let ws_msg = frame_res
                .map_err(|err| CoreError::Network(format!("read remote-auth frame: {err}")))?;

            let text = match ws_msg {
                wreq::Message::Text(t) => t.to_string(),
                wreq::Message::Close(_) => {
                    return Err(CoreError::Cancelled);
                }
                wreq::Message::Ping(payload) => {
                    let _ = ws_sink.send(wreq::Message::Pong(payload)).await;
                    continue;
                }
                _ => continue,
            };

            match decode_remote_auth_frame(&text)? {
                RemoteAuthServerFrame::Hello {
                    heartbeat_interval_ms,
                    timeout_ms: srv_timeout,
                } => {
                    timeout_ms = srv_timeout.max(5_000);
                    deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
                    let period = Duration::from_millis(heartbeat_interval_ms.max(1_000));
                    let mut interval = tokio::time::interval(period);
                    // First tick fires immediately; consume it so first heartbeat waits one full interval.
                    interval.tick().await;
                    heartbeat_interval = Some(interval);

                    let init_json = serde_json::to_string(&OutboundRemoteAuthFrame::Init {
                        encoded_public_key: keypair.encoded_public_key(),
                    })
                    .map_err(|err| CoreError::Protocol(format!("serialize remote-auth init: {err}")))?;
                    ws_sink
                        .send(wreq::Message::text(init_json))
                        .await
                        .map_err(|err| CoreError::Network(format!("send remote-auth init: {err}")))?;
                }
                RemoteAuthServerFrame::NonceProof { encrypted_nonce } => {
                    let nonce = keypair.solve_nonce_proof(&encrypted_nonce)?;
                    let proof_json = serde_json::to_string(&OutboundRemoteAuthFrame::NonceProof {
                        nonce: &nonce,
                    })
                    .map_err(|err| {
                        CoreError::Protocol(format!("serialize remote-auth nonce_proof: {err}"))
                    })?;
                    ws_sink
                        .send(wreq::Message::text(proof_json))
                        .await
                        .map_err(|err| {
                            CoreError::Network(format!("send remote-auth nonce_proof: {err}"))
                        })?;
                }
                RemoteAuthServerFrame::PendingRemoteInit { fingerprint } => {
                    let qr_url = format!("{REMOTE_AUTH_QR_PREFIX}{fingerprint}");
                    fingerprint_str = fingerprint.clone();
                    let _ = event_tx.send(RemoteAuthEvent::QrReady {
                        qr_url,
                        fingerprint,
                        timeout_ms,
                    });
                }
                RemoteAuthServerFrame::PendingTicket {
                    encrypted_user_payload,
                } => {
                    let pending_user = keypair.decrypt_user_payload(&encrypted_user_payload)?;
                    let _ = event_tx.send(RemoteAuthEvent::PendingConfirmation(pending_user));
                }
                RemoteAuthServerFrame::PendingLogin { ticket } => {
                    let _ = ws_sink.close().await;
                    let token = self
                        .exchange_ticket(&keypair, &fingerprint_str, &ticket)
                        .await?;
                    let _ = event_tx.send(RemoteAuthEvent::Authenticated(token.clone()));
                    return Ok(token);
                }
                RemoteAuthServerFrame::Cancel => {
                    let _ = ws_sink.close().await;
                    let _ = event_tx.send(RemoteAuthEvent::Cancelled);
                    return Err(CoreError::Cancelled);
                }
                RemoteAuthServerFrame::Ignored => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::DecodePublicKey;
    use rsa::RsaPublicKey;

    #[test]
    fn decode_remote_auth_frames_matches_discordo_vectors() {
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"hello","heartbeat_interval":100,"timeout_ms":200}"#)
                .expect("hello"),
            RemoteAuthServerFrame::Hello {
                heartbeat_interval_ms: 100,
                timeout_ms: 200
            }
        );
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"nonce_proof","encrypted_nonce":"abc"}"#)
                .expect("nonce_proof"),
            RemoteAuthServerFrame::NonceProof {
                encrypted_nonce: "abc".to_owned()
            }
        );
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"pending_remote_init","fingerprint":"fp123"}"#)
                .expect("pending_remote_init"),
            RemoteAuthServerFrame::PendingRemoteInit {
                fingerprint: "fp123".to_owned()
            }
        );
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"pending_ticket","encrypted_user_payload":"xyz"}"#)
                .expect("pending_ticket"),
            RemoteAuthServerFrame::PendingTicket {
                encrypted_user_payload: "xyz".to_owned()
            }
        );
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"pending_login","ticket":"tkt"}"#)
                .expect("pending_login"),
            RemoteAuthServerFrame::PendingLogin {
                ticket: "tkt".to_owned()
            }
        );
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"cancel"}"#).expect("cancel"),
            RemoteAuthServerFrame::Cancel
        );
        assert_eq!(
            decode_remote_auth_frame(r#"{"op":"heartbeat_ack"}"#).expect("heartbeat_ack"),
            RemoteAuthServerFrame::Ignored
        );
    }

    #[test]
    fn rsa_oaep_sha256_roundtrip_nonce_user_and_token() {
        let kp = RemoteAuthKeypair::generate().expect("keygen");
        let spki_bytes = BASE64_STANDARD
            .decode(kp.encoded_public_key())
            .expect("spki b64");
        let pub_key = RsaPublicKey::from_public_key_der(&spki_bytes).expect("valid SPKI DER");

        // Encrypt a synthetic 32-byte nonce using RSA-OAEP(SHA-256).
        let mut rng = rsa::rand_core::OsRng;
        let raw_nonce = b"synthetic-remote-auth-nonce-1234";
        let enc_nonce = pub_key
            .encrypt(&mut rng, Oaep::new::<Sha256>(), raw_nonce)
            .expect("encrypt nonce");
        let solved = kp
            .solve_nonce_proof(&BASE64_STANDARD.encode(&enc_nonce))
            .expect("solve nonce");
        assert_eq!(solved, URL_SAFE_NO_PAD.encode(raw_nonce));

        // Encrypt synthetic user payload `id:discriminator:avatar:username`.
        let user_plain = b"100000000000000001:0:avatarhash99:sysop_bbs";
        let enc_user = pub_key
            .encrypt(&mut rng, Oaep::new::<Sha256>(), user_plain)
            .expect("encrypt user");
        let parsed = kp
            .decrypt_user_payload(&BASE64_STANDARD.encode(&enc_user))
            .expect("decrypt user");
        assert_eq!(parsed.user_id, "100000000000000001");
        assert_eq!(parsed.discriminator, "0");
        assert_eq!(parsed.avatar_hash.as_deref(), Some("avatarhash99"));
        assert_eq!(parsed.username, "sysop_bbs");
        assert_eq!(parsed.display_handle(), "sysop_bbs");

        // Encrypt a synthetic non-token placeholder credential.
        let synthetic_cred = b"unit-test-ephemeral-session-value";
        let enc_tok = pub_key
            .encrypt(&mut rng, Oaep::new::<Sha256>(), synthetic_cred)
            .expect("encrypt token");
        let secret = kp
            .decrypt_token(&BASE64_STANDARD.encode(&enc_tok))
            .expect("decrypt token");
        assert_eq!(secret.expose_secret(), "unit-test-ephemeral-session-value");
        assert_eq!(format!("{secret:?}"), "SecretToken([REDACTED])");
    }
}
