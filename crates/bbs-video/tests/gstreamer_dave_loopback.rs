//! 100% headless end-to-end GStreamer + DAVE E2EE video loopback integration test.
//!
//! Runs without any Wayland/X11 display or XDG Desktop Portal:
//! `videotestsrc (num-buffers=15, pattern=smpte)` -> encoder (`vp8enc` or `x264enc`) ->
//! `davey` MLS video frame encryption -> `davey` MLS video frame decryption ->
//! decoder (`vp8dec` or `avdec_h264`) -> `videoconvert` -> `videoscale` ->
//! `AppSink` (`VideoSinkKind::InlineTerminal`) emitting packed RGBA8 [`VideoFrame`]s.

use std::time::Duration;

use bbs_core::{DaveSessionState, Snowflake, StreamQuality, StreamSourceKind, VideoFrame, VideoSinkKind};
use bbs_video::broadcast::{BroadcastConfig, BroadcastOutput, BroadcastPipeline};
use bbs_video::codec::{VideoCodec, probe_software_decoder, probe_software_encoder};
use bbs_video::dave::DaveVideoContext;
use bbs_video::watch::{WatchConfig, WatchPipeline};
use davey::{DAVE_PROTOCOL_VERSION, ProposalsOperationType};
use openmls::prelude::{tls_codec::Serialize as TlsSerialize, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use tokio::sync::{broadcast, mpsc};

const TEST_CHANNEL_ID: Snowflake = Snowflake::new(888_000_000_000_001);
const BROADCASTER_USER_ID: Snowflake = Snowflake::new(111_000_000_000_001);
const VIEWER_USER_ID: Snowflake = Snowflake::new(222_000_000_000_002);

/// In-memory MLS ExternalSender simulating Discord's Stream RTC Gateway for offline CI tests.
struct MockGatewayExternalSender {
    provider: OpenMlsRustCrypto,
    group_id: GroupId,
    signer: SignatureKeyPair,
    external_sender: ExternalSender,
}

impl MockGatewayExternalSender {
    fn new(channel_id: Snowflake) -> Self {
        let ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
        let signer = SignatureKeyPair::new(ciphersuite.signature_algorithm())
            .expect("create external sender signing key");
        let external_sender = ExternalSender::new(
            signer.public().into(),
            BasicCredential::new(vec![0x00, 0x01, 0x01, 0x00]).into(),
        );
        Self {
            provider: OpenMlsRustCrypto::default(),
            group_id: GroupId::from_slice(&channel_id.as_u64().to_be_bytes()),
            signer,
            external_sender,
        }
    }

    fn marshalled_external_sender(&self) -> Vec<u8> {
        self.external_sender
            .tls_serialize_detached()
            .expect("serialize external sender")
    }

    fn propose_add(&self, epoch: u64, key_package_bytes: &[u8]) -> Vec<u8> {
        let key_package = KeyPackageIn::tls_deserialize_exact_bytes(key_package_bytes)
            .expect("deserialize key package")
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .expect("validate key package");

        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            key_package,
            self.group_id.clone(),
            GroupEpoch::from(epoch),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .expect("build external add proposal");

        VLBytes::new(
            proposal
                .tls_serialize_detached()
                .expect("serialize external proposal"),
        )
        .tls_serialize_detached()
        .expect("serialize proposal list")
    }
}

/// Perform a genuine 2-party DAVE v1 MLS handshake between broadcaster and viewer contexts.
fn establish_active_dave_pair(
    channel_id: Snowflake,
    broadcaster_id: Snowflake,
    viewer_id: Snowflake,
) -> (DaveVideoContext, DaveVideoContext) {
    let gateway_sender = MockGatewayExternalSender::new(channel_id);
    let ext_bytes = gateway_sender.marshalled_external_sender();

    let broadcaster = DaveVideoContext::new(broadcaster_id, channel_id, DAVE_PROTOCOL_VERSION)
        .expect("init broadcaster DAVE context");
    let viewer = DaveVideoContext::new(viewer_id, channel_id, DAVE_PROTOCOL_VERSION)
        .expect("init viewer DAVE context");

    let _broadcaster_kp = broadcaster
        .set_external_sender(&ext_bytes)
        .expect("broadcaster set_external_sender");
    let viewer_kp = viewer
        .set_external_sender(&ext_bytes)
        .expect("viewer set_external_sender");

    let add_proposal = gateway_sender.propose_add(0, &viewer_kp);
    let roster = [broadcaster_id.as_u64(), viewer_id.as_u64()];

    let commit_welcome = broadcaster
        .process_proposals(ProposalsOperationType::APPEND, &add_proposal, Some(&roster))
        .expect("broadcaster process_proposals")
        .expect("commit + welcome produced");

    assert!(
        broadcaster
            .process_commit(&commit_welcome.commit)
            .expect("broadcaster process_commit")
    );
    viewer
        .process_welcome(
            commit_welcome
                .welcome
                .as_deref()
                .expect("welcome payload present"),
            Some(&roster),
        )
        .expect("viewer process_welcome");

    // Prepare & execute MLS epoch 1 transition on both endpoints.
    broadcaster
        .prepare_transition(1, DAVE_PROTOCOL_VERSION)
        .expect("broadcaster prepare_transition");
    viewer
        .prepare_transition(1, DAVE_PROTOCOL_VERSION)
        .expect("viewer prepare_transition");

    assert!(
        broadcaster
            .execute_transition(1)
            .expect("broadcaster execute_transition")
    );
    assert!(
        viewer
            .execute_transition(1)
            .expect("viewer execute_transition")
    );

    let b_state = broadcaster.session_state();
    let v_state = viewer.session_state();
    assert_eq!(b_state, v_state);
    match b_state {
        DaveSessionState::Active {
            epoch,
            protocol_version,
            verification_code,
        } => {
            assert_eq!(epoch, 1);
            assert_eq!(protocol_version, DAVE_PROTOCOL_VERSION);
            assert!(!verification_code.is_empty());
        }
        other => panic!("expected Active DAVE state after MLS handshake, got {other:?}"),
    }

    let b_pair_code = broadcaster
        .verification_code_for_user(viewer_id)
        .expect("broadcaster pairwise verification code");
    let v_pair_code = viewer
        .verification_code_for_user(broadcaster_id)
        .expect("viewer pairwise verification code");
    assert_eq!(b_pair_code, v_pair_code);

    (broadcaster, viewer)
}

/// Verify an RGBA8 buffer contains non-black SMPTE color content with real color diversity.
fn assert_valid_non_black_smpte_rgba(frame: &VideoFrame, expected_w: u32, expected_h: u32) {
    assert_eq!(frame.width, expected_w);
    assert_eq!(frame.height, expected_h);
    assert_eq!(
        frame.rgba.len(),
        (expected_w as usize) * (expected_h as usize) * 4
    );

    let mut non_black_pixels = 0usize;
    let mut sum_r = 0u64;
    let mut sum_g = 0u64;
    let mut sum_b = 0u64;
    let mut max_channel_delta = 0u8;

    for &[r, g, b, a] in frame.rgba.as_chunks::<4>().0 {
        assert_eq!(a, 255, "RGBA alpha channel should be opaque");
        if r > 16 || g > 16 || b > 16 {
            non_black_pixels += 1;
        }
        sum_r += u64::from(r);
        sum_g += u64::from(g);
        sum_b += u64::from(b);
        let max_c = r.max(g).max(b);
        let min_c = r.min(g).min(b);
        max_channel_delta = max_channel_delta.max(max_c - min_c);
    }

    let total_pixels = (expected_w as usize) * (expected_h as usize);
    assert!(
        non_black_pixels > total_pixels / 2,
        "frame {} looks mostly black ({non_black_pixels}/{total_pixels} lit pixels)",
        frame.seq
    );
    assert!(
        sum_r > 0 && sum_g > 0 && sum_b > 0,
        "frame {} missing RGB channel energy",
        frame.seq
    );
    assert!(
        max_channel_delta > 60,
        "frame {} lacks SMPTE color saturation (max RGB spread={max_channel_delta})",
        frame.seq
    );
}

async fn run_dave_loopback_for_codec(codec: VideoCodec, through_rtp: bool) -> Vec<VideoFrame> {
    let (broadcaster_dave, viewer_dave) =
        establish_active_dave_pair(TEST_CHANNEL_ID, BROADCASTER_USER_ID, VIEWER_USER_ID);

    let stream_key = format!("call:{TEST_CHANNEL_ID}:{BROADCASTER_USER_ID}");
    let width = 320u32;
    let height = 240u32;
    let num_buffers = 15i32;

    let (pkt_tx, mut pkt_rx) = mpsc::channel(32);
    let (frames_tx, mut frames_rx) = broadcast::channel(32);

    let broadcast_cfg = BroadcastConfig {
        stream_key: stream_key.clone(),
        source_kind: StreamSourceKind::SyntheticTestCard,
        quality: StreamQuality {
            width,
            height,
            fps: 30,
            max_bitrate_kbps: 1_500,
        },
        preferred_codec: Some(codec),
        prefer_software_encoder: true,
        synthetic_num_buffers: Some(num_buffers),
        synthetic_is_live: false,
        dave: broadcaster_dave.clone(),
    };

    let mut broadcast = BroadcastPipeline::start(
        broadcast_cfg,
        BroadcastOutput::EncodedChannel {
            tx: pkt_tx,
            through_rtp,
        },
    )
    .await
    .expect("start headless videotestsrc broadcast");

    assert_eq!(broadcast.encoder_selection().codec, codec);

    let mut watch_cfg = WatchConfig::new(
        stream_key.clone(),
        BROADCASTER_USER_ID,
        codec,
        VideoSinkKind::InlineTerminal,
        viewer_dave.clone(),
    );
    watch_cfg.inline_width = width;
    watch_cfg.inline_height = height;
    watch_cfg.inline_target_fps = 30;
    watch_cfg.enforce_wall_clock_throttle = false;
    watch_cfg.prefer_software_decoder = true;

    let mut watch =
        WatchPipeline::start_appsrc(watch_cfg, frames_tx).expect("start headless inline watch");

    // Forward every DAVE-encrypted encoded frame from broadcaster -> watch AppSrc.
    let mut encrypted_packets_seen = 0usize;
    let mut dave_marker_packets = 0usize;
    while let Ok(Some(pkt)) = tokio::time::timeout(Duration::from_secs(10), pkt_rx.recv()).await {
        assert_eq!(pkt.codec, codec);
        assert!(!pkt.payload.is_empty());
        if pkt.payload.ends_with(&[0xFA, 0xFA]) {
            dave_marker_packets += 1;
        }
        encrypted_packets_seen += 1;
        watch
            .push_encoded_frame(pkt.payload, pkt.pts, pkt.duration)
            .expect("push DAVE-encrypted frame into watch appsrc");
    }

    assert!(
        encrypted_packets_seen >= 12,
        "expected >= 12 encoded+DAVE-encrypted packets for {codec:?}, got {encrypted_packets_seen}"
    );
    assert!(
        dave_marker_packets >= 12,
        "expected DAVE magic footer 0xFAFA on encrypted {codec:?} frames (got {dave_marker_packets}/{encrypted_packets_seen})"
    );

    watch.end_of_stream().expect("signal watch EOS");

    // Drain decoded RGBA frames emitted by the InlineTerminal AppSink.
    let mut decoded_frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), frames_rx.recv()).await {
            Ok(Ok(frame)) => {
                decoded_frames.push(frame);
                if decoded_frames.len() >= encrypted_packets_seen {
                    break;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                panic!("test frame receiver lagged by {skipped} frames");
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            Err(_) => {
                if decoded_frames.len() >= 10 {
                    break;
                }
            }
        }
    }

    let b_stats = broadcaster_dave.stats();
    let v_stats = viewer_dave.stats();
    assert!(
        b_stats.encrypted_frames >= 12,
        "broadcaster should encrypt >= 12 frames, stats={b_stats:?}"
    );
    assert!(
        v_stats.decrypted_frames >= 12,
        "viewer should decrypt >= 12 frames, stats={v_stats:?}"
    );
    assert_eq!(b_stats.passthrough_frames, 0);
    assert_eq!(v_stats.passthrough_frames, 0);
    assert_eq!(b_stats.crypto_errors, 0);
    assert_eq!(v_stats.crypto_errors, 0);

    broadcast.stop().await;
    watch.stop().await;

    assert!(
        decoded_frames.len() >= 10,
        "expected 10+ decoded RGBA VideoFrames for {codec:?} (through_rtp={through_rtp}), got {}",
        decoded_frames.len()
    );

    let mut prev_seq = 0u64;
    for frame in &decoded_frames {
        assert_eq!(frame.stream_key, stream_key);
        assert!(frame.seq > prev_seq, "frame sequence numbers must increase");
        prev_seq = frame.seq;
        assert_valid_non_black_smpte_rgba(frame, width, height);
    }

    decoded_frames
}

#[tokio::test]
async fn gstreamer_dave_loopback_inline_terminal_smpte_end_to_end() {
    // Select whichever software codec pair (VP8 or H.264) is available on the host.
    let available_codec = if probe_software_encoder(Some(VideoCodec::Vp8))
        .is_ok_and(|e| e.codec == VideoCodec::Vp8)
        && probe_software_decoder(VideoCodec::Vp8).is_ok()
    {
        VideoCodec::Vp8
    } else if probe_software_encoder(Some(VideoCodec::H264))
        .is_ok_and(|e| e.codec == VideoCodec::H264)
        && probe_software_decoder(VideoCodec::H264).is_ok()
    {
        VideoCodec::H264
    } else {
        panic!("neither vp8enc/vp8dec nor x264enc/avdec_h264 available on host");
    };

    let frames = run_dave_loopback_for_codec(available_codec, false).await;
    assert!(frames.len() >= 10);
}

#[tokio::test]
async fn gstreamer_dave_loopback_h264_annex_b_and_rtp() {
    // Exercise H.264 Annex-B NAL-aware DAVE framing both direct and through real
    // `rtph264pay -> rtph264depay` RTP packetization whenever x264enc + avdec_h264 are installed.
    if probe_software_encoder(Some(VideoCodec::H264)).is_ok_and(|e| e.codec == VideoCodec::H264)
        && probe_software_decoder(VideoCodec::H264).is_ok()
    {
        let h264_frames = run_dave_loopback_for_codec(VideoCodec::H264, false).await;
        assert!(h264_frames.len() >= 10);

        let h264_rtp_frames = run_dave_loopback_for_codec(VideoCodec::H264, true).await;
        assert!(h264_rtp_frames.len() >= 10);
    }
}

#[test]
fn dave_rejects_tampered_video_ciphertext_without_panicking() {
    let (broadcaster_dave, viewer_dave) =
        establish_active_dave_pair(TEST_CHANNEL_ID, BROADCASTER_USER_ID, VIEWER_USER_ID);

    // Valid synthetic VP8 keyframe payload (>= 10 bytes header + frame body).
    let mut vp8_keyframe = vec![0x10u8, 0x00, 0x00, 0x9D, 0x01, 0x2A, 0x40, 0x01, 0xF0, 0x00];
    vp8_keyframe.extend_from_slice(&[0x55u8; 64]);

    let mut encrypted = broadcaster_dave
        .encrypt_video_frame(VideoCodec::Vp8, &vp8_keyframe)
        .expect("encrypt synthetic VP8 keyframe");
    assert_ne!(encrypted, vp8_keyframe);
    assert!(encrypted.ends_with(&[0xFA, 0xFA]));

    // Tamper with a ciphertext byte inside the encrypted section.
    encrypted[20] ^= 0xFF;

    let err = viewer_dave
        .decrypt_video_frame(BROADCASTER_USER_ID, &encrypted)
        .expect_err("tampered DAVE AEAD tag must fail cleanly");
    assert!(err.to_string().contains("DAVE video decrypt failed"));
    assert_eq!(viewer_dave.stats().crypto_errors, 1);
}

#[test]
fn dave_downgrade_to_protocol_version_zero_disables_state_and_allows_passthrough() {
    let (broadcaster_dave, viewer_dave) =
        establish_active_dave_pair(TEST_CHANNEL_ID, BROADCASTER_USER_ID, VIEWER_USER_ID);

    let mut vp8_frame = vec![0x10u8, 0x00, 0x00, 0x9D, 0x01, 0x2A, 0x40, 0x01, 0xF0, 0x00];
    vp8_frame.extend_from_slice(&[0x42u8; 48]);

    // Before downgrade: active epoch 1 encrypts & decrypts cleanly.
    let encrypted = broadcaster_dave
        .encrypt_video_frame(VideoCodec::Vp8, &vp8_frame)
        .expect("encrypt in epoch 1");
    assert_ne!(encrypted, vp8_frame);
    let decrypted = viewer_dave
        .decrypt_video_frame(BROADCASTER_USER_ID, &encrypted)
        .expect("decrypt in epoch 1");
    assert_eq!(decrypted, vp8_frame);

    // Prepare downgrade to protocol_version = 0 (Op 21): viewer immediately allows unencrypted
    // frames during transition without crypto errors.
    broadcaster_dave
        .prepare_transition(2, 0)
        .expect("broadcaster prepare downgrade");
    viewer_dave
        .prepare_transition(2, 0)
        .expect("viewer prepare downgrade");

    let transition_plain = viewer_dave
        .decrypt_video_frame(BROADCASTER_USER_ID, &vp8_frame)
        .expect("viewer must accept unencrypted frame during downgrade transition");
    assert_eq!(transition_plain, vp8_frame);

    // Execute downgrade (Op 22): session_state() must report Disabled (not Active epoch 0) and
    // both encrypt & decrypt must cleanly pass through unencrypted frames.
    assert!(
        broadcaster_dave
            .execute_transition(2)
            .expect("broadcaster execute downgrade")
    );
    assert!(
        viewer_dave
            .execute_transition(2)
            .expect("viewer execute downgrade")
    );

    assert_eq!(broadcaster_dave.session_state(), DaveSessionState::Disabled);
    assert_eq!(viewer_dave.session_state(), DaveSessionState::Disabled);
    assert!(!broadcaster_dave.can_encrypt());

    let post_enc = broadcaster_dave
        .encrypt_video_frame(VideoCodec::Vp8, &vp8_frame)
        .expect("broadcaster passthrough after downgrade");
    assert_eq!(post_enc, vp8_frame);
    let post_dec = viewer_dave
        .decrypt_video_frame(BROADCASTER_USER_ID, &post_enc)
        .expect("viewer passthrough after downgrade");
    assert_eq!(post_dec, vp8_frame);

    assert_eq!(broadcaster_dave.stats().crypto_errors, 0);
    assert_eq!(viewer_dave.stats().crypto_errors, 0);
    assert!(broadcaster_dave.stats().passthrough_frames >= 1);
    assert!(viewer_dave.stats().passthrough_frames >= 1);
}
