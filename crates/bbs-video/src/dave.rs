//! DAVE (Discord Audio & Video End-to-End Encryption) media frame crypto context and GStreamer pad probes.

use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bbs_core::{CoreError, CoreResult, DaveSessionState, Snowflake};
use davey::{CommitWelcome, DAVE_PROTOCOL_VERSION, DaveSession, MediaType, ProposalsOperationType};
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, trace, warn};

use crate::codec::VideoCodec;

/// Lock-free telemetry counters for DAVE video frame transforms.
#[derive(Debug, Default)]
pub struct DaveVideoMetrics {
    /// Total encoded video frames encrypted via DAVE.
    pub encrypted_frames: AtomicU64,
    /// Total encoded video frames decrypted via DAVE.
    pub decrypted_frames: AtomicU64,
    /// Total video frames forwarded in unencrypted passthrough mode.
    pub passthrough_frames: AtomicU64,
    /// Total encryption or decryption failures (dropped cleanly by pad probes).
    pub crypto_errors: AtomicU64,
}

/// Point-in-time snapshot of [`DaveVideoMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DaveVideoStats {
    /// Total encoded video frames encrypted via DAVE.
    pub encrypted_frames: u64,
    /// Total encoded video frames decrypted via DAVE.
    pub decrypted_frames: u64,
    /// Total video frames forwarded in unencrypted passthrough mode.
    pub passthrough_frames: u64,
    /// Total encryption or decryption failures.
    pub crypto_errors: u64,
}

impl DaveVideoMetrics {
    /// Take a consistent relaxed snapshot of counters.
    #[must_use]
    pub fn snapshot(&self) -> DaveVideoStats {
        DaveVideoStats {
            encrypted_frames: self.encrypted_frames.load(Ordering::Relaxed),
            decrypted_frames: self.decrypted_frames.load(Ordering::Relaxed),
            passthrough_frames: self.passthrough_frames.load(Ordering::Relaxed),
            crypto_errors: self.crypto_errors.load(Ordering::Relaxed),
        }
    }
}

struct DaveVideoInner {
    user_id: Snowflake,
    channel_id: Snowflake,
    session: Option<DaveSession>,
    allow_passthrough: bool,
    degraded_reason: Option<String>,
}

/// Thread-safe DAVE E2EE session context shared between Stream RTC signaling and GStreamer pad probes.
#[derive(Clone)]
pub struct DaveVideoContext {
    inner: Arc<Mutex<DaveVideoInner>>,
    metrics: Arc<DaveVideoMetrics>,
}

impl std::fmt::Debug for DaveVideoContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaveVideoContext")
            .field("state", &self.session_state())
            .field("stats", &self.stats())
            .finish()
    }
}

impl DaveVideoContext {
    /// Create a new DAVE E2EE video context for `(user_id, channel_id)` at `protocol_version`.
    ///
    /// Pass `protocol_version == 0` to start in disabled/passthrough mode until upgraded by
    /// `DAVE_PREPARE_EPOCH`.
    pub fn new(
        user_id: Snowflake,
        channel_id: Snowflake,
        protocol_version: u16,
    ) -> CoreResult<Self> {
        let session = if protocol_version == 0 {
            None
        } else {
            let ver = NonZeroU16::new(protocol_version).ok_or_else(|| {
                CoreError::Video("invalid DAVE protocol version 0".to_string())
            })?;
            let sess = DaveSession::new(ver, user_id.as_u64(), channel_id.as_u64(), None)
                .map_err(|err| CoreError::Video(format!("failed to init DAVE session: {err}")))?;
            Some(sess)
        };

        Ok(Self {
            inner: Arc::new(Mutex::new(DaveVideoInner {
                user_id,
                channel_id,
                session,
                allow_passthrough: protocol_version == 0,
                degraded_reason: None,
            })),
            metrics: Arc::new(DaveVideoMetrics::default()),
        })
    }

    /// Create an unencrypted passthrough context (used for non-DAVE sessions or local test cards).
    #[must_use]
    pub fn passthrough(user_id: Snowflake, channel_id: Snowflake) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DaveVideoInner {
                user_id,
                channel_id,
                session: None,
                allow_passthrough: true,
                degraded_reason: None,
            })),
            metrics: Arc::new(DaveVideoMetrics::default()),
        }
    }

    /// Wrap an already-initialized [`DaveSession`] (for example after an out-of-band MLS setup).
    #[must_use]
    pub fn from_session(session: DaveSession, allow_passthrough: bool) -> Self {
        let user_id = Snowflake::new(session.user_id());
        let channel_id = Snowflake::new(session.channel_id());
        Self {
            inner: Arc::new(Mutex::new(DaveVideoInner {
                user_id,
                channel_id,
                session: Some(session),
                allow_passthrough,
                degraded_reason: None,
            })),
            metrics: Arc::new(DaveVideoMetrics::default()),
        }
    }

    /// Return current lock-free encryption/decryption telemetry counters.
    #[must_use]
    pub fn stats(&self) -> DaveVideoStats {
        self.metrics.snapshot()
    }

    /// Project the current MLS session status into [`bbs_core::DaveSessionState`].
    #[must_use]
    pub fn session_state(&self) -> DaveSessionState {
        let Ok(guard) = self.inner.lock() else {
            return DaveSessionState::Degraded {
                reason: "DAVE session lock poisoned".to_string(),
            };
        };

        if let Some(reason) = &guard.degraded_reason {
            return DaveSessionState::Degraded {
                reason: reason.clone(),
            };
        }

        let Some(session) = &guard.session else {
            return DaveSessionState::Disabled;
        };

        if session.is_ready() {
            let epoch = session.epoch().map_or(0, |ep| ep.as_u64());
            let verification_code = session
                .voice_privacy_code()
                .unwrap_or_default()
                .to_string();
            DaveSessionState::Active {
                epoch,
                protocol_version: session.protocol_version().get(),
                verification_code,
            }
        } else {
            DaveSessionState::Handshaking
        }
    }

    /// Check whether this context currently has active encryption keys ready.
    #[must_use]
    pub fn can_encrypt(&self) -> bool {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.session.as_ref().map(DaveSession::can_encrypt))
            .unwrap_or(false)
    }

    /// Enable or disable passthrough mode across transitions.
    pub fn set_passthrough_mode(
        &self,
        enabled: bool,
        transition_expiry_secs: Option<u32>,
    ) -> CoreResult<()> {
        let mut guard = self.lock_inner()?;
        guard.allow_passthrough = enabled;
        if let Some(session) = guard.session.as_mut() {
            session.set_passthrough_mode(enabled, transition_expiry_secs);
        }
        Ok(())
    }

    /// Install the Voice Gateway external sender (Op 25) and generate our fresh MLS KeyPackage (Op 26).
    pub fn set_external_sender(&self, external_sender_bytes: &[u8]) -> CoreResult<Vec<u8>> {
        let mut guard = self.lock_inner()?;
        if guard.session.is_none() {
            let ver = NonZeroU16::new(DAVE_PROTOCOL_VERSION)
                .ok_or_else(|| CoreError::Video("invalid default DAVE version".to_string()))?;
            let sess = DaveSession::new(
                ver,
                guard.user_id.as_u64(),
                guard.channel_id.as_u64(),
                None,
            )
            .map_err(|err| CoreError::Video(format!("failed to create DAVE session: {err}")))?;
            guard.session = Some(sess);
        }

        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| CoreError::Video("DAVE session missing".to_string()))?;
        session
            .set_external_sender(external_sender_bytes)
            .map_err(|err| CoreError::Video(format!("DAVE set_external_sender failed: {err}")))?;
        let key_package = session
            .create_key_package()
            .map_err(|err| CoreError::Video(format!("DAVE create_key_package failed: {err}")))?;
        guard.degraded_reason = None;
        Ok(key_package)
    }

    /// Create a fresh serialized MLS KeyPackage (Op 26).
    pub fn create_key_package(&self) -> CoreResult<Vec<u8>> {
        let mut guard = self.lock_inner()?;
        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| CoreError::Video("DAVE session not initialized".to_string()))?;
        session
            .create_key_package()
            .map_err(|err| CoreError::Video(format!("DAVE create_key_package failed: {err}")))
    }

    /// Process incoming MLS proposals (Op 27), returning an optional Commit + Welcome for Op 28.
    pub fn process_proposals(
        &self,
        op_type: ProposalsOperationType,
        proposals: &[u8],
        expected_user_ids: Option<&[u64]>,
    ) -> CoreResult<Option<CommitWelcome>> {
        let mut guard = self.lock_inner()?;
        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| CoreError::Video("DAVE session not initialized".to_string()))?;
        session
            .process_proposals(op_type, proposals, expected_user_ids)
            .map_err(|err| CoreError::Video(format!("DAVE process_proposals failed: {err}")))
    }

    /// Process an announced MLS commit (Op 29). Returns `true` if caller should prepare transition.
    pub fn process_commit(&self, commit: &[u8]) -> CoreResult<bool> {
        let mut guard = self.lock_inner()?;
        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| CoreError::Video("DAVE session not initialized".to_string()))?;
        match session.process_commit(commit) {
            Ok(should_prepare) => {
                guard.degraded_reason = None;
                Ok(should_prepare)
            }
            Err(err) => {
                let msg = format!("DAVE process_commit failed: {err}");
                guard.degraded_reason = Some(msg.clone());
                Err(CoreError::Video(msg))
            }
        }
    }

    /// Process an incoming MLS Welcome (Op 30).
    pub fn process_welcome(
        &self,
        welcome: &[u8],
        recognized_user_ids: Option<&[u64]>,
    ) -> CoreResult<()> {
        let mut guard = self.lock_inner()?;
        let session = guard
            .session
            .as_mut()
            .ok_or_else(|| CoreError::Video("DAVE session not initialized".to_string()))?;
        match session.process_welcome(welcome, recognized_user_ids) {
            Ok(()) => {
                guard.degraded_reason = None;
                Ok(())
            }
            Err(err) => {
                let msg = format!("DAVE process_welcome failed: {err}");
                guard.degraded_reason = Some(msg.clone());
                Err(CoreError::Video(msg))
            }
        }
    }

    /// Prepare a key transition (Op 21 or immediately following Op 29 / Op 30).
    pub fn prepare_transition(&self, transition_id: u16, protocol_version: u16) -> CoreResult<()> {
        let mut guard = self.lock_inner()?;
        if protocol_version == 0 {
            guard.allow_passthrough = true;
        }
        let Some(session) = guard.session.as_mut() else {
            return Ok(());
        };
        session
            .prepare_transition(transition_id, protocol_version)
            .map_err(|err| CoreError::Video(format!("DAVE prepare_transition failed: {err}")))?;
        Ok(())
    }

    /// Execute a previously prepared key transition (Op 22).
    pub fn execute_transition(&self, transition_id: u16) -> CoreResult<bool> {
        let mut guard = self.lock_inner()?;
        let Some(session) = guard.session.as_mut() else {
            return Ok(false);
        };
        let executed = session
            .execute_transition(transition_id)
            .map_err(|err| CoreError::Video(format!("DAVE execute_transition failed: {err}")))?;
        if executed && session.can_encrypt() {
            guard.allow_passthrough = false;
            guard.degraded_reason = None;
        }
        Ok(executed)
    }

    /// Handle `DAVE_PREPARE_EPOCH` (Op 24). When `epoch == 1` and `protocol_version > 0`,
    /// re-initializes MLS state and returns a fresh KeyPackage to send in Op 26.
    pub fn prepare_epoch(
        &self,
        epoch: u64,
        protocol_version: u16,
    ) -> CoreResult<Option<Vec<u8>>> {
        let mut guard = self.lock_inner()?;
        if protocol_version == 0 {
            guard.allow_passthrough = true;
            if let Some(session) = guard.session.as_mut() {
                session
                    .reset()
                    .map_err(|err| CoreError::Video(format!("DAVE reset failed: {err}")))?;
            }
            return Ok(None);
        }

        if epoch != 1 {
            return Ok(None);
        }

        let ver = NonZeroU16::new(protocol_version)
            .ok_or_else(|| CoreError::Video("invalid DAVE protocol version".to_string()))?;
        let user_id = guard.user_id.as_u64();
        let channel_id = guard.channel_id.as_u64();

        if let Some(session) = guard.session.as_mut() {
            session
                .reinit(ver, user_id, channel_id, None)
                .map_err(|err| CoreError::Video(format!("DAVE reinit failed: {err}")))?;
            let kp = session
                .create_key_package()
                .map_err(|err| CoreError::Video(format!("DAVE create_key_package failed: {err}")))?;
            Ok(Some(kp))
        } else {
            let mut sess = DaveSession::new(ver, user_id, channel_id, None)
                .map_err(|err| CoreError::Video(format!("DAVE init failed: {err}")))?;
            let kp = sess
                .create_key_package()
                .map_err(|err| CoreError::Video(format!("DAVE create_key_package failed: {err}")))?;
            guard.session = Some(sess);
            Ok(Some(kp))
        }
    }

    /// Compute the pairwise verification code for a remote participant in the active MLS group.
    pub fn verification_code_for_user(&self, remote_user_id: Snowflake) -> CoreResult<String> {
        let guard = self.lock_inner()?;
        let session = guard
            .session
            .as_ref()
            .ok_or_else(|| CoreError::Video("DAVE session not active".to_string()))?;
        session
            .get_verification_code(remote_user_id.as_u64())
            .map_err(|err| CoreError::Video(format!("DAVE verification code unavailable: {err}")))
    }

    /// Encrypt an encoded video frame (Annex-B H.264, VP8, or VP9) using DAVE E2EE.
    pub fn encrypt_video_frame(&self, codec: VideoCodec, plaintext: &[u8]) -> CoreResult<Vec<u8>> {
        let mut guard = self.lock_inner()?;
        let allow_passthrough = guard.allow_passthrough;
        match guard.session.as_mut() {
            Some(session) if session.can_encrypt() => {
                let mut encrypted = Vec::with_capacity(plaintext.len().saturating_add(64));
                session
                    .encrypt_into(
                        MediaType::VIDEO,
                        codec.to_dave_codec(),
                        plaintext,
                        &mut encrypted,
                    )
                    .map_err(|err| {
                        self.metrics.crypto_errors.fetch_add(1, Ordering::Relaxed);
                        CoreError::Video(format!("DAVE video encrypt failed: {err}"))
                    })?;
                self.metrics
                    .encrypted_frames
                    .fetch_add(1, Ordering::Relaxed);
                Ok(encrypted)
            }
            _ if allow_passthrough => {
                self.metrics
                    .passthrough_frames
                    .fetch_add(1, Ordering::Relaxed);
                Ok(plaintext.to_vec())
            }
            _ => {
                self.metrics.crypto_errors.fetch_add(1, Ordering::Relaxed);
                Err(CoreError::Video(
                    "DAVE video encryption required but MLS session is not ready".to_string(),
                ))
            }
        }
    }

    /// Decrypt an incoming DAVE-encrypted video frame authored by `sender_user_id`.
    pub fn decrypt_video_frame(
        &self,
        sender_user_id: Snowflake,
        payload: &[u8],
    ) -> CoreResult<Vec<u8>> {
        let mut guard = self.lock_inner()?;
        let allow_passthrough = guard.allow_passthrough;
        match guard.session.as_mut() {
            Some(session) if session.is_ready() => {
                match session.decrypt(sender_user_id.as_u64(), MediaType::VIDEO, payload) {
                    Ok(plaintext) => {
                        self.metrics
                            .decrypted_frames
                            .fetch_add(1, Ordering::Relaxed);
                        Ok(plaintext)
                    }
                    Err(err) => {
                        self.metrics.crypto_errors.fetch_add(1, Ordering::Relaxed);
                        Err(CoreError::Video(format!("DAVE video decrypt failed: {err}")))
                    }
                }
            }
            _ if allow_passthrough => {
                self.metrics
                    .passthrough_frames
                    .fetch_add(1, Ordering::Relaxed);
                Ok(payload.to_vec())
            }
            _ => {
                self.metrics.crypto_errors.fetch_add(1, Ordering::Relaxed);
                Err(CoreError::Video(
                    "DAVE video decryption required but MLS session is not ready".to_string(),
                ))
            }
        }
    }

    fn lock_inner(&self) -> CoreResult<std::sync::MutexGuard<'_, DaveVideoInner>> {
        self.inner
            .lock()
            .map_err(|_| CoreError::Internal("DAVE video context lock poisoned".to_string()))
    }
}

/// Install a GStreamer buffer pad probe on `element`'s `"src"` pad to DAVE-encrypt every encoded
/// video frame in-flight while preserving PTS/DTS/duration/flags.
pub fn install_dave_encrypt_probe(
    element: &gst::Element,
    dave: DaveVideoContext,
    codec: VideoCodec,
) -> CoreResult<gst::PadProbeId> {
    let src_pad = element.static_pad("src").ok_or_else(|| {
        CoreError::Video(format!(
            "element `{}` has no static src pad for DAVE encryption",
            element.name()
        ))
    })?;

    let probe_id = src_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data else {
                return gst::PadProbeReturn::Ok;
            };

            let Ok(map) = buffer.map_readable() else {
                warn!("failed to map GStreamer video buffer for DAVE encryption");
                return gst::PadProbeReturn::Drop;
            };
            let encrypted = match dave.encrypt_video_frame(codec, map.as_slice()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(error = %err, codec = ?codec, "dropping unencryptable video frame");
                    return gst::PadProbeReturn::Drop;
                }
            };
            drop(map);

            trace!(
                codec = ?codec,
                plain_len = buffer.size(),
                enc_len = encrypted.len(),
                "encrypted encoded video frame with DAVE"
            );
            *buffer = replace_buffer_preserving_timing(buffer, encrypted);
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| {
            CoreError::Video("failed to install DAVE encryption pad probe".to_string())
        })?;

    debug!(element = %element.name(), codec = ?codec, "installed DAVE video encrypt pad probe");
    Ok(probe_id)
}

/// Install a GStreamer buffer pad probe on `element`'s `"src"` pad to DAVE-decrypt every incoming
/// encoded video frame authored by `sender_user_id` before decoding.
pub fn install_dave_decrypt_probe(
    element: &gst::Element,
    dave: DaveVideoContext,
    sender_user_id: Snowflake,
) -> CoreResult<gst::PadProbeId> {
    let src_pad = element.static_pad("src").ok_or_else(|| {
        CoreError::Video(format!(
            "element `{}` has no static src pad for DAVE decryption",
            element.name()
        ))
    })?;

    let probe_id = src_pad
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data else {
                return gst::PadProbeReturn::Ok;
            };

            let Ok(map) = buffer.map_readable() else {
                warn!("failed to map GStreamer video buffer for DAVE decryption");
                return gst::PadProbeReturn::Drop;
            };
            let decrypted = match dave.decrypt_video_frame(sender_user_id, map.as_slice()) {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(
                        error = %err,
                        sender = %sender_user_id,
                        "dropping undecryptable DAVE video frame"
                    );
                    return gst::PadProbeReturn::Drop;
                }
            };
            drop(map);

            trace!(
                sender = %sender_user_id,
                enc_len = buffer.size(),
                dec_len = decrypted.len(),
                "decrypted encoded video frame with DAVE"
            );
            *buffer = replace_buffer_preserving_timing(buffer, decrypted);
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| {
            CoreError::Video("failed to install DAVE decryption pad probe".to_string())
        })?;

    debug!(
        element = %element.name(),
        sender = %sender_user_id,
        "installed DAVE video decrypt pad probe"
    );
    Ok(probe_id)
}

fn replace_buffer_preserving_timing(original: &gst::Buffer, payload: Vec<u8>) -> gst::Buffer {
    let pts = original.pts();
    let dts = original.dts();
    let duration = original.duration();
    let offset = original.offset();
    let offset_end = original.offset_end();
    let flags = original.flags();

    let mut replacement = gst::Buffer::from_mut_slice(payload);
    if let Some(buf_ref) = replacement.get_mut() {
        buf_ref.set_pts(pts);
        buf_ref.set_dts(dts);
        buf_ref.set_duration(duration);
        buf_ref.set_offset(offset);
        buf_ref.set_offset_end(offset_end);
        buf_ref.set_flags(flags);
    }
    replacement
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_context_forwards_frames_without_error() {
        let ctx = DaveVideoContext::passthrough(Snowflake::new(1001), Snowflake::new(2002));
        assert_eq!(ctx.session_state(), DaveSessionState::Disabled);
        let sample = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E];
        let enc = ctx
            .encrypt_video_frame(VideoCodec::H264, &sample)
            .expect("passthrough encrypt");
        assert_eq!(enc, sample);
        let dec = ctx
            .decrypt_video_frame(Snowflake::new(1001), &enc)
            .expect("passthrough decrypt");
        assert_eq!(dec, sample);
        assert_eq!(ctx.stats().passthrough_frames, 2);
    }

    #[test]
    fn active_required_dave_rejects_unready_frames() {
        let ctx = DaveVideoContext::new(Snowflake::new(1001), Snowflake::new(2002), 1)
            .expect("create handshaking DAVE context");
        assert_eq!(ctx.session_state(), DaveSessionState::Handshaking);
        let err = ctx
            .encrypt_video_frame(VideoCodec::Vp8, &[0x10, 0x00, 0x00, 0x9d, 0x01, 0x2a])
            .expect_err("must not leak plaintext before MLS handshake completes");
        assert!(err.to_string().contains("not ready"));
        assert_eq!(ctx.stats().crypto_errors, 1);
    }
}
