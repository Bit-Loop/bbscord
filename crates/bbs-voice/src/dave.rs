//! Mandatory Discord Audio & Video End-to-End Encryption (DAVE / MLS) controller.
//!
//! Integrates [`davey::DaveSession`] with Discord Voice Gateway v8 DAVE text/binary opcodes:
//! - Op `21`: `DAVE_PROTOCOL_PREPARE_TRANSITION` (Server -> Client JSON)
//! - Op `22`: `DAVE_PROTOCOL_EXECUTE_TRANSITION` (Server -> Client JSON)
//! - Op `23`: `DAVE_PROTOCOL_READY_FOR_TRANSITION` (Client -> Server JSON)
//! - Op `24`: `DAVE_PROTOCOL_PREPARE_EPOCH` (Server -> Client JSON)
//! - Op `25`: `DAVE_MLS_EXTERNAL_SENDER_PACKAGE` (Server -> Client Binary)
//! - Op `26`: `DAVE_MLS_KEY_PACKAGE` (Client -> Server Binary)
//! - Op `27`: `DAVE_MLS_PROPOSALS` (Server -> Client Binary)
//! - Op `28`: `DAVE_MLS_COMMIT_WELCOME` (Client -> Server Binary)
//! - Op `29`: `DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION` (Server -> Client Binary)
//! - Op `30`: `DAVE_MLS_WELCOME` (Server -> Client Binary)
//! - Op `31`: `DAVE_MLS_INVALID_COMMIT_WELCOME` (Client -> Server JSON)

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroU16;

use bbs_core::model::{DaveSessionState, Snowflake};
use davey::{
    DAVE_PROTOCOL_VERSION, DaveSession, MediaType, OPUS_SILENCE_PACKET, ProposalsOperationType,
};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::error::{VoiceError, VoiceResult};

/// Maximum DAVE protocol version advertised in Voice Gateway `IDENTIFY` (`max_dave_protocol_version`).
pub const MAX_DAVE_PROTOCOL_VERSION: u16 = DAVE_PROTOCOL_VERSION;

/// Voice Gateway Op 21: `dave_protocol_prepare_transition` (S -> C JSON).
pub const OP_DAVE_PREPARE_TRANSITION: u8 = 21;
/// Voice Gateway Op 22: `dave_protocol_execute_transition` (S -> C JSON).
pub const OP_DAVE_EXECUTE_TRANSITION: u8 = 22;
/// Voice Gateway Op 23: `dave_protocol_ready_for_transition` (C -> S JSON).
pub const OP_DAVE_TRANSITION_READY: u8 = 23;
/// Voice Gateway Op 24: `dave_protocol_prepare_epoch` (S -> C JSON).
pub const OP_DAVE_PREPARE_EPOCH: u8 = 24;
/// Voice Gateway Op 25: `dave_mls_external_sender_package` (S -> C Binary).
pub const OP_DAVE_MLS_EXTERNAL_SENDER: u8 = 25;
/// Voice Gateway Op 26: `dave_mls_key_package` (C -> S Binary).
pub const OP_DAVE_MLS_KEY_PACKAGE: u8 = 26;
/// Voice Gateway Op 27: `dave_mls_proposals` (S -> C Binary).
pub const OP_DAVE_MLS_PROPOSALS: u8 = 27;
/// Voice Gateway Op 28: `dave_mls_commit_welcome` (C -> S Binary).
pub const OP_DAVE_MLS_COMMIT_WELCOME: u8 = 28;
/// Voice Gateway Op 29: `dave_mls_announce_commit_transition` (S -> C Binary).
pub const OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION: u8 = 29;
/// Voice Gateway Op 30: `dave_mls_welcome` (S -> C Binary).
pub const OP_DAVE_MLS_WELCOME: u8 = 30;
/// Voice Gateway Op 31: `dave_mls_invalid_commit_welcome` (C -> S JSON).
pub const OP_DAVE_MLS_INVALID_COMMIT_WELCOME: u8 = 31;

/// Inbound Voice Gateway DAVE signaling message (parsed from JSON or Binary WebSocket frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaveInboundMessage {
    /// Op 21: Prepare transition to `protocol_version` identified by `transition_id`.
    PrepareTransition {
        /// Transition identifier (`0` means immediate execution).
        transition_id: u16,
        /// Target DAVE protocol version (`0` disables E2EE / downgrades to transport-only).
        protocol_version: u16,
    },
    /// Op 22: Execute previously prepared transition `transition_id`.
    ExecuteTransition {
        /// Transition identifier.
        transition_id: u16,
    },
    /// Op 24: Prepare for MLS epoch change (`epoch == 1` resets local MLS group).
    PrepareEpoch {
        /// Target MLS epoch.
        epoch: u64,
        /// DAVE protocol version for this epoch.
        protocol_version: u16,
    },
    /// Op 25: Voice Gateway's MLS external sender credential & signature key.
    MlsExternalSender {
        /// Voice Gateway v8 sequence number.
        seq: u16,
        /// Raw TLS-serialized `ExternalSender`.
        external_sender: Vec<u8>,
    },
    /// Op 27: Queued MLS proposals (`APPEND` or `REVOKE`) broadcast by the Voice Gateway.
    MlsProposals {
        /// Voice Gateway v8 sequence number.
        seq: u16,
        /// `APPEND` (`0`) or `REVOKE` (`1`).
        operation_type: ProposalsOperationType,
        /// TLS-serialized `VLBytes` proposals payload.
        proposals: Vec<u8>,
    },
    /// Op 29: Broadcast of an accepted MLS Commit transitioning the group epoch.
    MlsAnnounceCommitTransition {
        /// Voice Gateway v8 sequence number.
        seq: u16,
        /// Transition identifier associated with this commit.
        transition_id: u16,
        /// TLS-serialized RFC 9420 `MLSMessage` (Commit).
        commit: Vec<u8>,
    },
    /// Op 30: Targeted MLS Welcome adding the local client into the group.
    MlsWelcome {
        /// Voice Gateway v8 sequence number.
        seq: u16,
        /// Transition identifier associated with this welcome.
        transition_id: u16,
        /// TLS-serialized RFC 9420 `Welcome`.
        welcome: Vec<u8>,
    },
}

/// Outbound Voice Gateway DAVE signaling message produced by [`DaveController`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaveOutboundMessage {
    /// Op 23 (JSON): Notify gateway that the local client is ready to execute `transition_id`.
    TransitionReady {
        /// Transition identifier.
        transition_id: u16,
    },
    /// Op 26 (Binary): Upload local client's serialized MLS `KeyPackage`.
    MlsKeyPackage(Vec<u8>),
    /// Op 28 (Binary): Upload local client's MLS `Commit` + optional `Welcome`.
    MlsCommitWelcome {
        /// Serialized MLS Commit message.
        commit: Vec<u8>,
        /// Optional serialized MLS Welcome message (present when proposals add members).
        welcome: Option<Vec<u8>>,
    },
    /// Op 31 (JSON): Report invalid/unprocessable Commit or Welcome to trigger group recovery.
    InvalidCommitWelcome {
        /// Transition identifier that failed.
        transition_id: u16,
    },
}

/// Wire encoding of an outbound Voice Gateway WebSocket frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaveWireFrame {
    /// UTF-8 JSON text frame (Op 23 / Op 31).
    Text(String),
    /// Binary frame (`[opcode: u8][payload...]`, Op 26 / Op 28).
    Binary(Vec<u8>),
}

impl DaveOutboundMessage {
    /// Encode this outbound DAVE signaling message into a Voice Gateway WebSocket frame.
    #[must_use]
    pub fn to_wire_frame(&self) -> DaveWireFrame {
        match self {
            Self::TransitionReady { transition_id } => {
                let payload = json!({
                    "op": OP_DAVE_TRANSITION_READY,
                    "d": {
                        "transition_id": transition_id,
                    }
                });
                DaveWireFrame::Text(payload.to_string())
            }
            Self::MlsKeyPackage(key_package) => {
                let mut frame = Vec::with_capacity(1 + key_package.len());
                frame.push(OP_DAVE_MLS_KEY_PACKAGE);
                frame.extend_from_slice(key_package);
                DaveWireFrame::Binary(frame)
            }
            Self::MlsCommitWelcome { commit, welcome } => {
                let welcome_len = welcome.as_ref().map_or(0, Vec::len);
                let mut frame = Vec::with_capacity(1 + commit.len() + welcome_len);
                frame.push(OP_DAVE_MLS_COMMIT_WELCOME);
                frame.extend_from_slice(commit);
                if let Some(welcome_bytes) = welcome {
                    frame.extend_from_slice(welcome_bytes);
                }
                DaveWireFrame::Binary(frame)
            }
            Self::InvalidCommitWelcome { transition_id } => {
                let payload = json!({
                    "op": OP_DAVE_MLS_INVALID_COMMIT_WELCOME,
                    "d": {
                        "transition_id": transition_id,
                    }
                });
                DaveWireFrame::Text(payload.to_string())
            }
        }
    }
}

/// Parse a Voice Gateway v8 server-to-client binary DAVE frame (`[seq: u16 BE][op: u8][payload...]`).
pub fn parse_binary_dave_frame(bytes: &[u8]) -> VoiceResult<DaveInboundMessage> {
    if bytes.len() < 3 {
        return Err(VoiceError::Dave(format!(
            "binary DAVE gateway frame too short ({} bytes, minimum 3)",
            bytes.len()
        )));
    }

    let seq = u16::from_be_bytes([bytes[0], bytes[1]]);
    let opcode = bytes[2];
    let body = &bytes[3..];

    match opcode {
        OP_DAVE_MLS_EXTERNAL_SENDER => Ok(DaveInboundMessage::MlsExternalSender {
            seq,
            external_sender: body.to_vec(),
        }),
        OP_DAVE_MLS_PROPOSALS => {
            let Some(&op_byte) = body.first() else {
                return Err(VoiceError::Dave(
                    "Op 27 (MLS proposals) missing operation_type byte".to_string(),
                ));
            };
            let operation_type = match op_byte {
                0 => ProposalsOperationType::APPEND,
                1 => ProposalsOperationType::REVOKE,
                other => {
                    return Err(VoiceError::Dave(format!(
                        "Op 27 (MLS proposals) unknown operation_type {other}"
                    )));
                }
            };
            Ok(DaveInboundMessage::MlsProposals {
                seq,
                operation_type,
                proposals: body[1..].to_vec(),
            })
        }
        OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION => {
            if body.len() < 2 {
                return Err(VoiceError::Dave(
                    "Op 29 (announce commit transition) missing 2-byte transition_id".to_string(),
                ));
            }
            let transition_id = u16::from_be_bytes([body[0], body[1]]);
            Ok(DaveInboundMessage::MlsAnnounceCommitTransition {
                seq,
                transition_id,
                commit: body[2..].to_vec(),
            })
        }
        OP_DAVE_MLS_WELCOME => {
            if body.len() < 2 {
                return Err(VoiceError::Dave(
                    "Op 30 (MLS welcome) missing 2-byte transition_id".to_string(),
                ));
            }
            let transition_id = u16::from_be_bytes([body[0], body[1]]);
            Ok(DaveInboundMessage::MlsWelcome {
                seq,
                transition_id,
                welcome: body[2..].to_vec(),
            })
        }
        other => Err(VoiceError::Dave(format!(
            "unexpected server->client binary DAVE opcode {other}"
        ))),
    }
}

/// Parse a Voice Gateway JSON DAVE opcode (`21`, `22`, `24`).
///
/// Returns `Ok(None)` if `op` is not a DAVE JSON opcode.
pub fn parse_json_dave_frame(op: u8, data: &Value) -> VoiceResult<Option<DaveInboundMessage>> {
    match op {
        OP_DAVE_PREPARE_TRANSITION => {
            let transition_id = extract_u16(data, "transition_id")?;
            let protocol_version = extract_u16(data, "protocol_version")?;
            Ok(Some(DaveInboundMessage::PrepareTransition {
                transition_id,
                protocol_version,
            }))
        }
        OP_DAVE_EXECUTE_TRANSITION => {
            let transition_id = extract_u16(data, "transition_id")?;
            Ok(Some(DaveInboundMessage::ExecuteTransition {
                transition_id,
            }))
        }
        OP_DAVE_PREPARE_EPOCH => {
            let epoch = data
                .get("epoch")
                .and_then(Value::as_u64)
                .ok_or_else(|| VoiceError::Dave("Op 24 missing u64 field `epoch`".to_string()))?;
            let protocol_version = extract_u16(data, "protocol_version")?;
            Ok(Some(DaveInboundMessage::PrepareEpoch {
                epoch,
                protocol_version,
            }))
        }
        _ => Ok(None),
    }
}

fn extract_u16(data: &Value, field: &str) -> VoiceResult<u16> {
    let raw = data
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| VoiceError::Dave(format!("missing integer field `{field}`")))?;
    u16::try_from(raw)
        .map_err(|_| VoiceError::Dave(format!("field `{field}` value {raw} out of u16 range")))
}

/// Format a raw numeric DAVE displayable code into hyphen-separated 5-digit groups
/// (e.g. `"48291-07314-92650-11842-..."`).
#[must_use]
pub fn format_verification_code(raw_digits: &str) -> String {
    if raw_digits.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(raw_digits.len() + raw_digits.len() / 5);
    for (idx, ch) in raw_digits.chars().enumerate() {
        if idx > 0 && idx % 5 == 0 {
            out.push('-');
        }
        out.push(ch);
    }
    out
}

/// Stateful DAVE E2EE controller managing the MLS group lifecycle, frame crypto, and SAS codes.
pub struct DaveController {
    user_id: Snowflake,
    channel_id: Snowflake,
    protocol_version: u16,
    session: Option<DaveSession>,
    recognized_users: HashSet<u64>,
    last_external_sender: Option<Vec<u8>>,
    key_package_sent: bool,
    pending_transitions: HashMap<u16, u16>,
    degraded_reason: Option<String>,
}

impl std::fmt::Debug for DaveController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaveController")
            .field("user_id", &self.user_id)
            .field("channel_id", &self.channel_id)
            .field("protocol_version", &self.protocol_version)
            .field("recognized_users_count", &self.recognized_users.len())
            .field("state", &self.state())
            .finish()
    }
}

impl DaveController {
    /// Construct a new DAVE controller for `(user_id, channel_id)`.
    #[must_use]
    pub fn new(user_id: Snowflake, channel_id: Snowflake) -> Self {
        let mut recognized_users = HashSet::new();
        recognized_users.insert(user_id.as_u64());
        Self {
            user_id,
            channel_id,
            protocol_version: 0,
            session: None,
            recognized_users,
            last_external_sender: None,
            key_package_sent: false,
            pending_transitions: HashMap::new(),
            degraded_reason: None,
        }
    }

    /// Return the local user Snowflake.
    #[must_use]
    pub const fn user_id(&self) -> Snowflake {
        self.user_id
    }

    /// Return the channel Snowflake (MLS group ID).
    #[must_use]
    pub const fn channel_id(&self) -> Snowflake {
        self.channel_id
    }

    /// Record a participant reported by Voice Gateway `CLIENTS_CONNECT` (Op 11) or `SPEAKING` (Op 5).
    pub fn add_recognized_user(&mut self, user_id: Snowflake) {
        self.recognized_users.insert(user_id.as_u64());
    }

    /// Remove a participant reported by Voice Gateway `CLIENT_DISCONNECT` (Op 13).
    pub fn remove_recognized_user(&mut self, user_id: Snowflake) {
        if user_id != self.user_id {
            self.recognized_users.remove(&user_id.as_u64());
        }
    }

    /// Return the sorted snapshot of recognized user IDs (always including local user).
    #[must_use]
    pub fn recognized_user_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.recognized_users.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Initialize or disable DAVE upon receiving Voice Gateway `SESSION_DESCRIPTION` (Op 4).
    pub fn on_session_description(
        &mut self,
        dave_protocol_version: u16,
    ) -> VoiceResult<Vec<DaveOutboundMessage>> {
        let prev_version = self.protocol_version;
        self.protocol_version = dave_protocol_version;
        self.degraded_reason = None;

        if dave_protocol_version == 0 {
            if let Some(session) = self.session.as_mut() {
                let _ = session.reset();
                session.set_passthrough_mode(true, Some(0));
            }
            self.key_package_sent = false;
            return Ok(Vec::new());
        }

        // If Op 25 (External Sender) arrived before Op 4 and already created and emitted a valid
        // key package for this exact protocol version, do not overwrite it in OpenMLS storage.
        if self.key_package_sent
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.protocol_version().get() == dave_protocol_version)
            && prev_version == dave_protocol_version
        {
            return Ok(Vec::new());
        }

        let kp = self.ensure_initialized_and_create_key_package(dave_protocol_version)?;
        Ok(vec![DaveOutboundMessage::MlsKeyPackage(kp)])
    }

    /// Process an inbound Voice Gateway DAVE signaling message (`Op 21..=30`) and return any
    /// outbound messages (`Op 23`, `Op 26`, `Op 28`, `Op 31`) that must be sent back immediately.
    pub fn handle_gateway_message(
        &mut self,
        message: DaveInboundMessage,
    ) -> VoiceResult<Vec<DaveOutboundMessage>> {
        match message {
            DaveInboundMessage::MlsExternalSender {
                seq: _,
                external_sender,
            } => {
                self.last_external_sender = Some(external_sender.clone());
                let target_ver = if self.protocol_version == 0 {
                    self.protocol_version = MAX_DAVE_PROTOCOL_VERSION;
                    MAX_DAVE_PROTOCOL_VERSION
                } else {
                    self.protocol_version
                };
                self.ensure_session(target_ver)?;

                if let Some(session) = self.session.as_mut() {
                    session
                        .set_external_sender(&external_sender)
                        .map_err(|e| VoiceError::Dave(format!("set_external_sender: {e}")))?;
                }

                if !self.key_package_sent {
                    let kp = self.create_key_package()?;
                    self.key_package_sent = true;
                    return Ok(vec![DaveOutboundMessage::MlsKeyPackage(kp)]);
                }
                Ok(Vec::new())
            }

            DaveInboundMessage::PrepareEpoch {
                epoch,
                protocol_version,
            } => {
                self.protocol_version = protocol_version;
                if protocol_version == 0 {
                    if let Some(session) = self.session.as_mut() {
                        session.set_passthrough_mode(true, Some(10));
                        let _ = session.reset();
                    }
                    self.key_package_sent = false;
                    return Ok(Vec::new());
                }

                if epoch == 1 {
                    let kp = self.reinit_and_create_key_package(protocol_version)?;
                    return Ok(vec![DaveOutboundMessage::MlsKeyPackage(kp)]);
                }
                Ok(Vec::new())
            }

            DaveInboundMessage::MlsProposals {
                seq: _,
                operation_type,
                proposals,
            } => {
                let recognized = self.recognized_user_ids();
                let session = self.session.as_mut().ok_or_else(|| {
                    VoiceError::Dave("received MLS proposals before DAVE session init".to_string())
                })?;

                match session.process_proposals(operation_type, &proposals, Some(&recognized)) {
                    Ok(Some(cw)) => Ok(vec![DaveOutboundMessage::MlsCommitWelcome {
                        commit: cw.commit,
                        welcome: cw.welcome,
                    }]),
                    Ok(None) => Ok(Vec::new()),
                    Err(err) => {
                        warn!("DAVE process_proposals failed: {err}");
                        self.degraded_reason = Some(format!("proposal processing failed: {err}"));
                        Err(VoiceError::Dave(format!("process_proposals: {err}")))
                    }
                }
            }

            DaveInboundMessage::MlsAnnounceCommitTransition {
                seq: _,
                transition_id,
                commit,
            } => {
                let Some(session) = self.session.as_mut() else {
                    debug!("ignoring MLS commit before DAVE session init");
                    return Ok(Vec::new());
                };

                match session.process_commit(&commit) {
                    Ok(true) => {
                        let ver = self.protocol_version.max(1);
                        session
                            .prepare_transition(transition_id, ver)
                            .map_err(|e| VoiceError::Dave(format!("prepare_transition: {e}")))?;
                        self.degraded_reason = None;
                        if transition_id == 0 {
                            Ok(Vec::new())
                        } else {
                            self.pending_transitions.insert(transition_id, ver);
                            Ok(vec![DaveOutboundMessage::TransitionReady { transition_id }])
                        }
                    }
                    Ok(false) => Ok(Vec::new()),
                    Err(err) => {
                        warn!("DAVE invalid commit for transition {transition_id}: {err}");
                        self.degraded_reason = Some(format!("invalid MLS commit: {err}"));
                        let mut outbound = vec![DaveOutboundMessage::InvalidCommitWelcome {
                            transition_id,
                        }];
                        if self.protocol_version > 0 {
                            let kp = self.reinit_and_create_key_package(self.protocol_version)?;
                            outbound.push(DaveOutboundMessage::MlsKeyPackage(kp));
                        }
                        Ok(outbound)
                    }
                }
            }

            DaveInboundMessage::MlsWelcome {
                seq: _,
                transition_id,
                welcome,
            } => {
                let recognized = self.recognized_user_ids();
                let Some(session) = self.session.as_mut() else {
                    return Err(VoiceError::Dave(
                        "received MLS welcome before DAVE session init".to_string(),
                    ));
                };

                match session.process_welcome(&welcome, Some(&recognized)) {
                    Ok(()) => {
                        let ver = self.protocol_version.max(1);
                        session
                            .prepare_transition(transition_id, ver)
                            .map_err(|e| VoiceError::Dave(format!("prepare_transition: {e}")))?;
                        self.degraded_reason = None;
                        if transition_id == 0 {
                            Ok(Vec::new())
                        } else {
                            self.pending_transitions.insert(transition_id, ver);
                            Ok(vec![DaveOutboundMessage::TransitionReady { transition_id }])
                        }
                    }
                    Err(err) => {
                        warn!("DAVE invalid welcome for transition {transition_id}: {err}");
                        self.degraded_reason = Some(format!("invalid MLS welcome: {err}"));
                        let mut outbound = vec![DaveOutboundMessage::InvalidCommitWelcome {
                            transition_id,
                        }];
                        if self.protocol_version > 0 {
                            let kp = self.reinit_and_create_key_package(self.protocol_version)?;
                            outbound.push(DaveOutboundMessage::MlsKeyPackage(kp));
                        }
                        Ok(outbound)
                    }
                }
            }

            DaveInboundMessage::PrepareTransition {
                transition_id,
                protocol_version,
            } => {
                if let Some(session) = self.session.as_mut() {
                    if protocol_version == 0 {
                        session.set_passthrough_mode(true, Some(10));
                    }
                    session
                        .prepare_transition(transition_id, protocol_version)
                        .map_err(|e| VoiceError::Dave(format!("prepare_transition: {e}")))?;
                }
                if transition_id == 0 {
                    self.protocol_version = protocol_version;
                    if protocol_version == 0 {
                        self.key_package_sent = false;
                    }
                    Ok(Vec::new())
                } else {
                    self.pending_transitions
                        .insert(transition_id, protocol_version);
                    Ok(vec![DaveOutboundMessage::TransitionReady { transition_id }])
                }
            }

            DaveInboundMessage::ExecuteTransition { transition_id } => {
                if let Some(target_version) = self.pending_transitions.remove(&transition_id) {
                    self.protocol_version = target_version;
                }
                if let Some(session) = self.session.as_mut() {
                    let executed = session
                        .execute_transition(transition_id)
                        .map_err(|e| VoiceError::Dave(format!("execute_transition: {e}")))?;
                    if executed {
                        if self.protocol_version == 0 {
                            session.set_passthrough_mode(true, Some(0));
                            self.key_package_sent = false;
                        } else {
                            session.set_passthrough_mode(false, Some(10));
                            self.degraded_reason = None;
                        }
                    }
                }
                Ok(Vec::new())
            }
        }
    }

    /// Encrypt an outbound Opus frame with DAVE E2EE prior to RTP transport AEAD encryption.
    ///
    /// - When DAVE is disabled (`protocol_version == 0`), passes the Opus frame through unchanged.
    /// - Opus silence packets (`[0xF8, 0xFF, 0xFE]`) are passed through unencrypted per DAVE spec.
    /// - When DAVE is negotiating (`Handshaking`) and not yet ready, refuses to emit unencrypted
    ///   media frames by returning `Err(VoiceError::Dave(...))`.
    pub fn encrypt_opus<'a>(&mut self, opus_frame: &'a [u8]) -> VoiceResult<Cow<'a, [u8]>> {
        if self.protocol_version == 0 {
            return Ok(Cow::Borrowed(opus_frame));
        }
        if opus_frame == OPUS_SILENCE_PACKET {
            return Ok(Cow::Borrowed(opus_frame));
        }
        let session = self.session.as_mut().ok_or_else(|| {
            VoiceError::Dave("DAVE session not initialized; refusing unencrypted send".to_string())
        })?;
        if !session.can_encrypt() {
            return Err(VoiceError::Dave(
                "DAVE MLS handshake incomplete; refusing unencrypted send".to_string(),
            ));
        }
        session
            .encrypt_opus(opus_frame)
            .map_err(|e| VoiceError::Dave(format!("encrypt_opus failed: {e}")))
    }

    /// Decrypt an inbound DAVE-protected Opus frame from `sender_user_id` after RTP AEAD decryption.
    pub fn decrypt_opus(
        &mut self,
        sender_user_id: Snowflake,
        payload: &[u8],
    ) -> VoiceResult<Vec<u8>> {
        if self.protocol_version == 0 || payload == OPUS_SILENCE_PACKET {
            return Ok(payload.to_vec());
        }
        let session = self.session.as_mut().ok_or_else(|| {
            VoiceError::Dave("DAVE session not initialized; cannot decrypt frame".to_string())
        })?;
        session
            .decrypt(sender_user_id.as_u64(), MediaType::AUDIO, payload)
            .map_err(|e| {
                VoiceError::Dave(format!("decrypt_opus for user {sender_user_id} failed: {e}"))
            })
    }

    /// Compute the [`DaveSessionState`] projection exposed to `bbs-core` and the TUI.
    #[must_use]
    pub fn state(&self) -> DaveSessionState {
        if self.protocol_version == 0 {
            return DaveSessionState::Disabled;
        }
        if let Some(session) = &self.session
            && session.is_ready()
            && session.can_encrypt()
            && let Some(epoch) = session.epoch()
        {
            let raw_code = session.voice_privacy_code().unwrap_or_default();
            return DaveSessionState::Active {
                epoch: epoch.as_u64(),
                protocol_version: session.protocol_version().get(),
                verification_code: format_verification_code(raw_code),
            };
        }
        if let Some(reason) = &self.degraded_reason {
            return DaveSessionState::Degraded {
                reason: reason.clone(),
            };
        }
        DaveSessionState::Handshaking
    }

    /// Return the formatted 30-digit group voice privacy code (derived from the MLS epoch authenticator).
    #[must_use]
    pub fn voice_privacy_code(&self) -> Option<String> {
        let raw = self.session.as_ref()?.voice_privacy_code()?;
        Some(format_verification_code(raw))
    }

    /// Compute the formatted 45-digit pairwise SAS verification fingerprint between the local user
    /// and `peer_user_id` per the DAVE whitepaper (`generateDisplayableCode(getPairwiseFingerprint(0, peer), 45, 5)`).
    pub fn peer_verification_code(&self, peer_user_id: Snowflake) -> VoiceResult<String> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| VoiceError::Dave("DAVE session not initialized".to_string()))?;
        let raw = session
            .get_verification_code(peer_user_id.as_u64())
            .map_err(|e| VoiceError::Dave(format!("get_verification_code failed: {e}")))?;
        Ok(format_verification_code(&raw))
    }

    /// Returns `true` if the DAVE MLS session is active and `user_id` is present in the group roster.
    #[must_use]
    pub fn is_user_in_verified_group(&self, user_id: Snowflake) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        if !session.is_ready() || !session.can_encrypt() {
            return false;
        }
        session
            .get_user_ids()
            .is_some_and(|ids| ids.contains(&user_id.as_u64()))
    }

    /// Returns `true` when the DAVE session can currently encrypt outbound audio frames.
    #[must_use]
    pub fn can_encrypt(&self) -> bool {
        self.protocol_version == 0 || self.session.as_ref().is_some_and(DaveSession::can_encrypt)
    }

    fn ensure_session(&mut self, protocol_version: u16) -> VoiceResult<()> {
        let version = NonZeroU16::new(protocol_version).ok_or_else(|| {
            VoiceError::Dave("cannot initialize DAVE session with protocol version 0".to_string())
        })?;
        if self.session.is_none() {
            let session =
                DaveSession::new(version, self.user_id.as_u64(), self.channel_id.as_u64(), None)
                    .map_err(|e| VoiceError::Dave(format!("DaveSession::new failed: {e}")))?;
            self.session = Some(session);
        }
        Ok(())
    }

    fn create_key_package(&mut self) -> VoiceResult<Vec<u8>> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| VoiceError::Dave("DAVE session not initialized".to_string()))?;
        session
            .create_key_package()
            .map_err(|e| VoiceError::Dave(format!("create_key_package failed: {e}")))
    }

    fn ensure_initialized_and_create_key_package(
        &mut self,
        protocol_version: u16,
    ) -> VoiceResult<Vec<u8>> {
        self.ensure_session(protocol_version)?;
        if let Some(ext) = self.last_external_sender.clone()
            && let Some(session) = self.session.as_mut()
            && session.group().is_none()
        {
            session
                .set_external_sender(&ext)
                .map_err(|e| VoiceError::Dave(format!("set_external_sender: {e}")))?;
        }
        let kp = self.create_key_package()?;
        self.key_package_sent = true;
        Ok(kp)
    }

    fn reinit_and_create_key_package(&mut self, protocol_version: u16) -> VoiceResult<Vec<u8>> {
        let version = NonZeroU16::new(protocol_version).ok_or_else(|| {
            VoiceError::Dave("cannot reinit DAVE session with protocol version 0".to_string())
        })?;
        if let Some(session) = self.session.as_mut() {
            session
                .reinit(version, self.user_id.as_u64(), self.channel_id.as_u64(), None)
                .map_err(|e| VoiceError::Dave(format!("DaveSession::reinit failed: {e}")))?;
        } else {
            self.ensure_session(protocol_version)?;
            if let Some(ext) = self.last_external_sender.clone()
                && let Some(session) = self.session.as_mut()
            {
                session
                    .set_external_sender(&ext)
                    .map_err(|e| VoiceError::Dave(format!("set_external_sender: {e}")))?;
            }
        }
        self.pending_transitions.clear();
        let kp = self.create_key_package()?;
        self.key_package_sent = true;
        Ok(kp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_and_json_dave_wire_framing() {
        // Op 25: [seq=7][op=25][payload]
        let mut op25 = vec![0x00, 0x07, OP_DAVE_MLS_EXTERNAL_SENDER, 0xDE, 0xAD];
        let parsed = parse_binary_dave_frame(&op25).expect("op 25");
        assert_eq!(
            parsed,
            DaveInboundMessage::MlsExternalSender {
                seq: 7,
                external_sender: vec![0xDE, 0xAD],
            }
        );

        // Op 27: [seq=8][op=27][APPEND=0][proposals...]
        let op27 = vec![0x00, 0x08, OP_DAVE_MLS_PROPOSALS, 0x00, 0x01, 0x02];
        assert_eq!(
            parse_binary_dave_frame(&op27).expect("op 27"),
            DaveInboundMessage::MlsProposals {
                seq: 8,
                operation_type: ProposalsOperationType::APPEND,
                proposals: vec![0x01, 0x02],
            }
        );

        // Op 29: [seq=9][op=29][transition_id=42][commit...]
        let op29 = vec![0x00, 0x09, OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION, 0x00, 42, 0xAA];
        assert_eq!(
            parse_binary_dave_frame(&op29).expect("op 29"),
            DaveInboundMessage::MlsAnnounceCommitTransition {
                seq: 9,
                transition_id: 42,
                commit: vec![0xAA],
            }
        );

        // Op 30: [seq=10][op=30][transition_id=42][welcome...]
        let op30 = vec![0x00, 0x0A, OP_DAVE_MLS_WELCOME, 0x00, 42, 0xBB];
        assert_eq!(
            parse_binary_dave_frame(&op30).expect("op 30"),
            DaveInboundMessage::MlsWelcome {
                seq: 10,
                transition_id: 42,
                welcome: vec![0xBB],
            }
        );

        // Truncated frames must fail cleanly without panicking.
        op25.truncate(2);
        assert!(parse_binary_dave_frame(&op25).is_err());
        assert!(parse_binary_dave_frame(&[0, 1, OP_DAVE_MLS_PROPOSALS]).is_err());
        assert!(parse_binary_dave_frame(&[0, 1, OP_DAVE_MLS_PROPOSALS, 99]).is_err());
        assert!(parse_binary_dave_frame(&[0, 1, OP_DAVE_MLS_WELCOME, 0]).is_err());

        // JSON opcodes 21, 22, 24
        let op21 = parse_json_dave_frame(
            OP_DAVE_PREPARE_TRANSITION,
            &json!({"transition_id": 5, "protocol_version": 1}),
        )
        .expect("op 21");
        assert_eq!(
            op21,
            Some(DaveInboundMessage::PrepareTransition {
                transition_id: 5,
                protocol_version: 1,
            })
        );

        let op22 = parse_json_dave_frame(OP_DAVE_EXECUTE_TRANSITION, &json!({"transition_id": 5}))
            .expect("op 22");
        assert_eq!(
            op22,
            Some(DaveInboundMessage::ExecuteTransition { transition_id: 5 })
        );

        let op24 = parse_json_dave_frame(
            OP_DAVE_PREPARE_EPOCH,
            &json!({"epoch": 1, "protocol_version": 1}),
        )
        .expect("op 24");
        assert_eq!(
            op24,
            Some(DaveInboundMessage::PrepareEpoch {
                epoch: 1,
                protocol_version: 1,
            })
        );

        // Outbound encoding: Op 26, Op 28, Op 23, Op 31
        assert_eq!(
            DaveOutboundMessage::MlsKeyPackage(vec![1, 2, 3]).to_wire_frame(),
            DaveWireFrame::Binary(vec![OP_DAVE_MLS_KEY_PACKAGE, 1, 2, 3])
        );
        assert_eq!(
            DaveOutboundMessage::MlsCommitWelcome {
                commit: vec![10, 11],
                welcome: Some(vec![20, 21]),
            }
            .to_wire_frame(),
            DaveWireFrame::Binary(vec![OP_DAVE_MLS_COMMIT_WELCOME, 10, 11, 20, 21])
        );
        assert_eq!(
            format_verification_code("48291073149265011842"),
            "48291-07314-92650-11842"
        );
    }
}
