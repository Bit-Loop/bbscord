//! 100% offline headless integration tests verifying:
//! 1. Real two-peer `davey` MLS handshake + 440 Hz sine -> Opus -> DAVE -> RTP AEAD -> RTP decrypt
//!    -> DAVE decrypt -> Opus decode -> soft-clip mixer, Goertzel 440 Hz energy check,
//!    multi-layer tamper rejection, and matching group + pairwise SAS verification fingerprints.
//! 2. Full in-memory Voice Gateway v8 WebSocket + 74-byte UDP IP Discovery + UDP SFU loopback
//!    joining a Group DM call (`guild_id: None`) via `VoiceEngine::join_voice` with live DAVE
//!    opcodes (21..=31) and real-time Braille VU `VoiceAudioLevels` emission — zero soundcard needed!

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bbs_core::events::BackendEvent;
use bbs_core::model::{DaveSessionState, Snowflake, VoiceConnectionState};
use bbs_core::traits::VoiceEngine;
use bbs_voice::dave::{
    DaveController, DaveInboundMessage, DaveOutboundMessage, OP_DAVE_EXECUTE_TRANSITION,
    OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION, OP_DAVE_MLS_COMMIT_WELCOME,
    OP_DAVE_MLS_EXTERNAL_SENDER, OP_DAVE_MLS_KEY_PACKAGE, OP_DAVE_MLS_PROPOSALS,
    OP_DAVE_MLS_WELCOME, OP_DAVE_TRANSITION_READY, parse_binary_dave_frame,
};
use bbs_voice::session::{
    VOICE_OP_CLIENTS_CONNECT, VOICE_OP_HEARTBEAT, VOICE_OP_HEARTBEAT_ACK, VOICE_OP_HELLO,
    VOICE_OP_IDENTIFY, VOICE_OP_READY, VOICE_OP_SELECT_PROTOCOL, VOICE_OP_SESSION_DESCRIPTION,
    VOICE_OP_SPEAKING,
};
use bbs_voice::{
    AudioBackendMode, AudioMixer, DISCOVERY_PACKET_LEN, DISCOVERY_PAYLOAD_LEN,
    DISCOVERY_TYPE_RESPONSE, DiscordVoiceEngine, EncodedMicPacket, FRAME_SAMPLES,
    FRAME_SAMPLES_PER_CHANNEL, InputMode, MicCaptureProcessor, RtpPacketizer, RtpTransportCipher,
    SAMPLE_RATE, TransportEncryptionMode, VoiceEngineConfig, VoiceResult, VoiceServerUpdateEvent,
    VoiceSignalingBridge, VoiceStateUpdateCommand, VoiceStateUpdateEvent, compute_rms_db,
};
use futures::{SinkExt, StreamExt};
use openmls::prelude::{tls_codec::Serialize as _, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{Value, json};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use zeroize::Zeroizing;

/// Local MLS External Sender playing the role of Discord's Voice Gateway SFU.
struct LocalExternalSender {
    provider: OpenMlsRustCrypto,
    group_id: GroupId,
    signer: SignatureKeyPair,
    external_sender: ExternalSender,
}

impl LocalExternalSender {
    fn new(channel_id: Snowflake) -> Self {
        let ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
        let signer =
            SignatureKeyPair::new(ciphersuite.signature_algorithm()).expect("signature keypair");
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

    fn propose_add_payload(&self, epoch: u64, key_package_bytes: &[u8]) -> Vec<u8> {
        let kp = KeyPackageIn::tls_deserialize_exact_bytes(key_package_bytes)
            .expect("deserialize key package")
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .expect("validate key package");

        let msg = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            kp,
            self.group_id.clone(),
            GroupEpoch::from(epoch),
            &self.signer,
            SenderExtensionIndex::new(0),
        )
        .expect("external add proposal");

        VLBytes::new(msg.tls_serialize_detached().expect("serialize proposal"))
            .tls_serialize_detached()
            .expect("serialize VLBytes")
    }
}

fn generate_440hz_stereo_frame(frame_idx: usize, amplitude: f32) -> [f32; FRAME_SAMPLES] {
    let mut pcm = [0.0f32; FRAME_SAMPLES];
    let base = frame_idx * FRAME_SAMPLES_PER_CHANNEL;
    for i in 0..FRAME_SAMPLES_PER_CHANNEL {
        let t = (base + i) as f32 / (SAMPLE_RATE as f32);
        let s = amplitude * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
        pcm[i * 2] = s;
        pcm[i * 2 + 1] = s;
    }
    pcm
}

/// Goertzel single-bin DFT power at `target_hz` on left-channel samples of interleaved stereo PCM.
fn goertzel_power_left(pcm_stereo: &[f32], target_hz: f64) -> f64 {
    let n = pcm_stereo.len() / 2;
    if n == 0 {
        return 0.0;
    }
    let omega = 2.0 * std::f64::consts::PI * target_hz / f64::from(SAMPLE_RATE);
    let coeff = 2.0 * omega.cos();
    let mut s_prev = 0.0f64;
    let mut s_prev2 = 0.0f64;
    for frame in pcm_stereo.chunks_exact(2) {
        let s = f64::from(frame[0]) + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let power = s_prev2 * s_prev2 + s_prev * s_prev - coeff * s_prev * s_prev2;
    power / ((n as f64) * (n as f64))
}

fn build_server_binary_frame(seq: u16, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + payload.len());
    out.extend_from_slice(&seq.to_be_bytes());
    out.push(opcode);
    out.extend_from_slice(payload);
    out
}

#[test]
fn dave_opus_rtp_aead_mixer_loopback() {
    let channel_id = Snowflake(300_000_000_000_000_200);
    let user_a = Snowflake(100_000_000_000_000_001);
    let user_b = Snowflake(100_000_000_000_000_002);

    let ext_sender = LocalExternalSender::new(channel_id);
    let mut dave_a = DaveController::new(user_a, channel_id);
    let mut dave_b = DaveController::new(user_b, channel_id);

    dave_a.add_recognized_user(user_b);
    dave_b.add_recognized_user(user_a);

    // 1. SESSION_DESCRIPTION (dave_protocol_version = 1) -> both emit Op 26 KeyPackages.
    let out_a = dave_a.on_session_description(1).expect("a session desc");
    let out_b = dave_b.on_session_description(1).expect("b session desc");
    assert_eq!(out_a.len(), 1);
    let DaveOutboundMessage::MlsKeyPackage(kp_b) = &out_b[0] else {
        panic!("expected Peer B Op 26 MlsKeyPackage");
    };

    // Before MLS handshake completes, DAVE MUST refuse unencrypted audio!
    assert!(dave_a.encrypt_opus(&[1, 2, 3, 4]).is_err());

    // 2. Deliver Op 25 (External Sender) parsed via binary gateway framing.
    let ext_bytes = ext_sender.marshalled_external_sender();
    let op25_a = parse_binary_dave_frame(&build_server_binary_frame(
        1,
        OP_DAVE_MLS_EXTERNAL_SENDER,
        &ext_bytes,
    ))
    .expect("parse op25 a");
    let op25_b = parse_binary_dave_frame(&build_server_binary_frame(
        1,
        OP_DAVE_MLS_EXTERNAL_SENDER,
        &ext_bytes,
    ))
    .expect("parse op25 b");
    dave_a.handle_gateway_message(op25_a).expect("a set ext");
    dave_b.handle_gateway_message(op25_b).expect("b set ext");

    // 3. External sender proposes adding Peer B -> Op 27 to Peer A -> Peer A produces Op 28 Commit+Welcome!
    let proposals = ext_sender.propose_add_payload(0, kp_b);
    let mut op27_body = Vec::with_capacity(1 + proposals.len());
    op27_body.push(0u8); // APPEND
    op27_body.extend_from_slice(&proposals);
    let op27 = parse_binary_dave_frame(&build_server_binary_frame(
        2,
        OP_DAVE_MLS_PROPOSALS,
        &op27_body,
    ))
    .expect("parse op27");

    let commit_out = dave_a
        .handle_gateway_message(op27)
        .expect("a process proposals");
    assert_eq!(commit_out.len(), 1);
    let DaveOutboundMessage::MlsCommitWelcome { commit, welcome } = &commit_out[0] else {
        panic!("expected Op 28 MlsCommitWelcome");
    };
    let welcome = welcome.as_ref().expect("welcome for added peer B");

    // 4. Broadcast Op 29 (Announce Commit Transition) to Peer A and Op 30 (Welcome) to Peer B.
    let transition_id = 7u16;
    let mut op29_body = Vec::with_capacity(2 + commit.len());
    op29_body.extend_from_slice(&transition_id.to_be_bytes());
    op29_body.extend_from_slice(commit);
    let op29 = parse_binary_dave_frame(&build_server_binary_frame(
        3,
        OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION,
        &op29_body,
    ))
    .expect("parse op29");
    let ready_a = dave_a
        .handle_gateway_message(op29)
        .expect("a process commit");
    assert_eq!(
        ready_a,
        vec![DaveOutboundMessage::TransitionReady { transition_id }]
    );

    let mut op30_body = Vec::with_capacity(2 + welcome.len());
    op30_body.extend_from_slice(&transition_id.to_be_bytes());
    op30_body.extend_from_slice(welcome);
    let op30 = parse_binary_dave_frame(&build_server_binary_frame(
        3,
        OP_DAVE_MLS_WELCOME,
        &op30_body,
    ))
    .expect("parse op30");
    let ready_b = dave_b
        .handle_gateway_message(op30)
        .expect("b process welcome");
    assert_eq!(
        ready_b,
        vec![DaveOutboundMessage::TransitionReady { transition_id }]
    );

    // 5. Execute Transition (Op 22) on both peers -> Active E2EE!
    dave_a
        .handle_gateway_message(DaveInboundMessage::ExecuteTransition { transition_id })
        .expect("a execute transition");
    dave_b
        .handle_gateway_message(DaveInboundMessage::ExecuteTransition { transition_id })
        .expect("b execute transition");

    let state_a = dave_a.state();
    let state_b = dave_b.state();
    assert_eq!(state_a, state_b, "both peers must agree on DaveSessionState");
    match &state_a {
        DaveSessionState::Active {
            epoch,
            protocol_version,
            verification_code,
        } => {
            assert_eq!(*epoch, 1);
            assert_eq!(*protocol_version, 1);
            assert!(
                verification_code.len() >= 30,
                "expected 30-digit formatted voice privacy code, got {verification_code:?}"
            );
        }
        other => panic!("expected DaveSessionState::Active, got {other:?}"),
    }

    // Verify BOTH group voice privacy code AND 45-digit pairwise SAS fingerprint match!
    assert_eq!(dave_a.voice_privacy_code(), dave_b.voice_privacy_code());
    let sas_a_to_b = dave_a
        .peer_verification_code(user_b)
        .expect("peer A SAS for B");
    let sas_b_to_a = dave_b
        .peer_verification_code(user_a)
        .expect("peer B SAS for A");
    assert_eq!(
        sas_a_to_b, sas_b_to_a,
        "pairwise SAS verification fingerprints must match identically on both peers"
    );
    assert!(dave_a.is_user_in_verified_group(user_b));
    assert!(dave_b.is_user_in_verified_group(user_a));

    // 6. Full media pipeline across BOTH AEAD modes:
    // 440 Hz sine -> MicCaptureProcessor (HPF + VAD + Opus) -> DAVE encrypt -> RTP AEAD ->
    // RTP decrypt -> DAVE decrypt -> Opus decode -> AudioMixer (tanh soft clip).
    let transport_key = [0x5Au8; 32];
    for mode in [
        TransportEncryptionMode::AeadXChaCha20Poly1305RtpSize,
        TransportEncryptionMode::AeadAes256GcmRtpSize,
    ] {
        let mut mic = MicCaptureProcessor::new(InputMode::VoiceActivity {
            threshold_db: -45.0,
        })
        .expect("mic");
        let mut packetizer = RtpPacketizer::with_initial(4242, 10, 0);
        let mut tx_cipher = RtpTransportCipher::new(mode, &transport_key).expect("tx cipher");
        let rx_cipher = RtpTransportCipher::new(mode, &transport_key).expect("rx cipher");
        let mut mixer = AudioMixer::new(2);
        mixer.bind_ssrc(4242, user_a);

        let mut recovered_pcm = Vec::new();
        let mut mix_out = [0.0f32; FRAME_SAMPLES];
        let mut encrypted_samples_checked = false;
        for frame_idx in 0..12 {
            let pcm = generate_440hz_stereo_frame(frame_idx, 0.55);
            let mic_res = mic.process_frame(&pcm).expect("mic process");
            assert!(mic_res.speaking);
            let EncodedMicPacket::Audio(opus_pkt) = mic_res.packet else {
                panic!("expected Opus Audio packet");
            };

            let dave_pkt = dave_a
                .encrypt_opus(&opus_pkt)
                .expect("dave encrypt")
                .into_owned();
            assert_ne!(
                dave_pkt, opus_pkt,
                "DAVE ciphertext must differ from plaintext Opus"
            );

            let header = packetizer.next_opus_header();
            let mut rtp_wire = tx_cipher
                .encrypt_packet(&header, &dave_pkt)
                .expect("rtp encrypt");

            if !encrypted_samples_checked {
                encrypted_samples_checked = true;

                // Tamper test 1: flip a bit inside the RTP AEAD ciphertext -> MUST fail RTP decrypt.
                let tamper_idx = rtp_wire.len() - 8;
                rtp_wire[tamper_idx] ^= 0xFF;
                assert!(
                    rx_cipher.decrypt_packet(&rtp_wire).is_err(),
                    "tampered RTP AEAD packet must be rejected ({mode:?})"
                );
                rtp_wire[tamper_idx] ^= 0xFF;

                // Tamper test 2: flip a bit inside DAVE ciphertext, re-seal under valid RTP AEAD ->
                // RTP decrypt succeeds, DAVE decrypt MUST reject cleanly!
                let mut tampered_dave = dave_pkt.clone();
                tampered_dave[0] ^= 0x40;
                let tampered_wire = tx_cipher
                    .encrypt_packet(&header, &tampered_dave)
                    .expect("re-encrypt tampered dave");
                let rtp_ok = rx_cipher
                    .decrypt_packet(&tampered_wire)
                    .expect("outer RTP valid");
                assert!(
                    dave_b.decrypt_opus(user_a, &rtp_ok.payload).is_err(),
                    "tampered DAVE E2EE payload must be rejected cleanly"
                );
            }

            let dec_rtp = rx_cipher.decrypt_packet(&rtp_wire).expect("rtp decrypt");
            assert_eq!(dec_rtp.header, header);

            let dec_opus = dave_b
                .decrypt_opus(user_a, &dec_rtp.payload)
                .expect("dave decrypt");
            assert_eq!(dec_opus, opus_pkt);

            mixer
                .push_opus_packet(
                    dec_rtp.header.ssrc,
                    Some(user_a),
                    dec_rtp.header.sequence,
                    dec_opus,
                )
                .expect("push opus");

            // Drain steadily as packets arrive (simulating 20 ms playback cadence).
            if mixer.mix_frame(&mut mix_out) > 0 {
                recovered_pcm.extend_from_slice(&mix_out);
            }
        }

        // Flush remaining frames held in the jitter cushion.
        for _ in 0..6 {
            if mixer.mix_frame_flushing(&mut mix_out) > 0 {
                recovered_pcm.extend_from_slice(&mix_out);
            }
        }

        assert!(
            recovered_pcm.len() >= FRAME_SAMPLES * 10,
            "expected >= 10 mixed frames, got {} samples",
            recovered_pcm.len()
        );

        let rms_db = compute_rms_db(&recovered_pcm);
        assert!(
            rms_db > -18.0,
            "expected strong recovered 440 Hz RMS energy, got {rms_db} dBFS ({mode:?})"
        );

        // Spectral verification: power at 440 Hz must vastly dominate power at 1200 Hz!
        let p440 = goertzel_power_left(&recovered_pcm, 440.0);
        let p1200 = goertzel_power_left(&recovered_pcm, 1200.0);
        assert!(
            p440 > 0.01 && p440 > p1200 * 50.0,
            "expected clean 440 Hz sine recovery: p440={p440}, p1200={p1200}"
        );
    }

    // 7. Invalid MLS commit triggers Op 31 (`InvalidCommitWelcome`) + fresh Op 26 recovery!
    let bogus_commit = DaveInboundMessage::MlsAnnounceCommitTransition {
        seq: 99,
        transition_id: 44,
        commit: vec![0xDE, 0xAD, 0xBE, 0xEF],
    };
    let recovery = dave_b
        .handle_gateway_message(bogus_commit)
        .expect("recovery should not panic");
    assert!(matches!(
        recovery.first(),
        Some(DaveOutboundMessage::InvalidCommitWelcome { transition_id: 44 })
    ));
    assert!(matches!(
        recovery.get(1),
        Some(DaveOutboundMessage::MlsKeyPackage(_))
    ));
}

struct MockSignalingHub {
    ws_endpoint: String,
    recorded_commands: Mutex<Vec<VoiceStateUpdateCommand>>,
    target_engine: Mutex<Option<Arc<DiscordVoiceEngine>>>,
    user_id: Snowflake,
    session_id: String,
}

impl MockSignalingHub {
    fn new(ws_endpoint: String, user_id: Snowflake, session_id: String) -> Arc<Self> {
        Arc::new(Self {
            ws_endpoint,
            recorded_commands: Mutex::new(Vec::new()),
            target_engine: Mutex::new(None),
            user_id,
            session_id,
        })
    }

    fn attach_engine(&self, engine: Arc<DiscordVoiceEngine>) {
        *self.target_engine.lock().expect("lock") = Some(engine);
    }
}

#[async_trait]
impl VoiceSignalingBridge for MockSignalingHub {
    async fn send_voice_state_update(&self, cmd: VoiceStateUpdateCommand) -> VoiceResult<()> {
        self.recorded_commands
            .lock()
            .expect("lock")
            .push(cmd.clone());

        if let Some(channel_id) = cmd.channel_id {
            let engine = self.target_engine.lock().expect("lock").clone();
            if let Some(eng) = engine {
                let guild_id = cmd.guild_id;
                let user_id = self.user_id;
                let session_id = self.session_id.clone();
                let endpoint = self.ws_endpoint.clone();
                tokio::spawn(async move {
                    eng.handle_voice_state_update(VoiceStateUpdateEvent {
                        guild_id,
                        channel_id: Some(channel_id),
                        user_id,
                        session_id,
                        self_mute: cmd.self_mute,
                        self_deaf: cmd.self_deaf,
                        mute: false,
                        deaf: false,
                        self_stream: false,
                        display_name: None,
                    })
                    .await;
                    eng.handle_voice_server_update(VoiceServerUpdateEvent {
                        guild_id,
                        channel_id: Some(channel_id),
                        endpoint: Some(endpoint),
                        token: Zeroizing::new("synthetic-loopback-voice-token".to_string()),
                    })
                    .await;
                });
            }
        }
        Ok(())
    }
}

struct ConnectedPeer {
    user_id: Snowflake,
    ssrc: u32,
    tx: mpsc::UnboundedSender<WsMessage>,
    udp_addr: Option<SocketAddr>,
    key_package: Option<Vec<u8>>,
}

#[tokio::test]
async fn full_voice_gateway_v8_group_dm_call_loopback() {
    let dm_channel_id = Snowflake(300_000_000_000_000_200);
    let user_a = Snowflake(100_000_000_000_000_001);
    let user_b = Snowflake(100_000_000_000_000_002);
    let secret_key = [0x33u8; 32];

    // Bind local UDP SFU socket (handles 74-byte IP discovery + RTP relay).
    let sfu_udp = Arc::new(
        UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("bind udp"),
    );
    let sfu_udp_addr = sfu_udp.local_addr().expect("udp addr");

    // Bind local Voice Gateway v8 WebSocket listener.
    let ws_listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind ws");
    let ws_addr = ws_listener.local_addr().expect("ws addr");
    let ws_endpoint = format!("ws://{ws_addr}");

    let peers: Arc<Mutex<Vec<ConnectedPeer>>> = Arc::new(Mutex::new(Vec::new()));
    let external_sender = Arc::new(LocalExternalSender::new(dm_channel_id));
    let observed_server_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // Spawn local UDP SFU (discovery + packet relay between peers).
    let udp_peers = Arc::clone(&peers);
    let sfu_udp_task = tokio::spawn({
        let sfu_udp = Arc::clone(&sfu_udp);
        async move {
            let mut buf = [0u8; 4096];
            while let Ok((n, src)) = sfu_udp.recv_from(&mut buf).await {
                if n == DISCOVERY_PACKET_LEN && buf[0..2] == [0x00, 0x01] {
                    // Build 74-byte UDP IP Discovery response.
                    let ssrc = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    let mut resp = [0u8; DISCOVERY_PACKET_LEN];
                    resp[0..2].copy_from_slice(&DISCOVERY_TYPE_RESPONSE.to_be_bytes());
                    resp[2..4].copy_from_slice(&DISCOVERY_PAYLOAD_LEN.to_be_bytes());
                    resp[4..8].copy_from_slice(&ssrc.to_be_bytes());
                    let ip_bytes = src.ip().to_string();
                    resp[8..8 + ip_bytes.len()].copy_from_slice(ip_bytes.as_bytes());
                    resp[72..74].copy_from_slice(&src.port().to_be_bytes());
                    let _ = sfu_udp.send_to(&resp, src).await;

                    if let Ok(mut guard) = udp_peers.lock()
                        && let Some(peer) = guard.iter_mut().find(|p| p.ssrc == ssrc)
                    {
                        peer.udp_addr = Some(src);
                    }
                } else {
                    // Relay RTP datagrams to all other connected peers!
                    let targets: Vec<SocketAddr> = udp_peers
                        .lock()
                        .ok()
                        .map(|g| {
                            g.iter()
                                .filter_map(|p| p.udp_addr.filter(|&a| a != src))
                                .collect()
                        })
                        .unwrap_or_default();
                    for dst in targets {
                        let _ = sfu_udp.send_to(&buf[..n], dst).await;
                    }
                }
            }
        }
    });

    // Spawn local Voice Gateway v8 coordinator handling 2 peers + real DAVE MLS handshake!
    let ws_task = tokio::spawn({
        let peers = Arc::clone(&peers);
        let ext_sender = Arc::clone(&external_sender);
        let observed_server_ids = Arc::clone(&observed_server_ids);
        async move {
            let (kp_notify_tx, mut kp_notify_rx) = mpsc::unbounded_channel::<(Snowflake, Vec<u8>)>();
            let (cw_notify_tx, mut cw_notify_rx) = mpsc::unbounded_channel::<(Vec<u8>, Option<Vec<u8>>)>();
            let (ready_tx, mut ready_rx) = mpsc::unbounded_channel::<u16>();

            // DAVE MLS orchestrator task driving Op 27 -> Op 28 -> Op 29/30 -> Op 23 -> Op 22!
            let dave_orch = tokio::spawn({
                let peers = Arc::clone(&peers);
                let ext_sender = Arc::clone(&ext_sender);
                async move {
                    let mut received_kps: Vec<(Snowflake, Vec<u8>)> = Vec::new();
                    while let Some(kp) = kp_notify_rx.recv().await {
                        received_kps.push(kp);
                        if received_kps.len() == 2 {
                            break;
                        }
                    }
                    // Send Op 25 External Sender to both peers.
                    let ext_bytes = ext_sender.marshalled_external_sender();
                    let op25 = WsMessage::Binary(
                        build_server_binary_frame(10, OP_DAVE_MLS_EXTERNAL_SENDER, &ext_bytes)
                            .into(),
                    );
                    if let Ok(guard) = peers.lock() {
                        for p in guard.iter() {
                            let _ = p.tx.send(op25.clone());
                        }
                    }

                    // Propose adding user_b (the second peer) to user_a!
                    let kp_b = received_kps
                        .iter()
                        .find(|(uid, _)| *uid == user_b)
                        .map(|(_, kp)| kp.clone())
                        .expect("user_b kp");
                    let prop = ext_sender.propose_add_payload(0, &kp_b);
                    let mut op27_body = Vec::with_capacity(1 + prop.len());
                    op27_body.push(0u8); // APPEND
                    op27_body.extend_from_slice(&prop);
                    let op27 = WsMessage::Binary(
                        build_server_binary_frame(11, OP_DAVE_MLS_PROPOSALS, &op27_body).into(),
                    );
                    if let Ok(guard) = peers.lock()
                        && let Some(pa) = guard.iter().find(|p| p.user_id == user_a)
                    {
                        let _ = pa.tx.send(op27);
                    }

                    // Receive Commit+Welcome (Op 28) from user_a.
                    let (commit, welcome) = cw_notify_rx.recv().await.expect("commit welcome");
                    let welcome = welcome.expect("welcome bytes");
                    let tid = 1u16;

                    let mut op29_body = Vec::with_capacity(2 + commit.len());
                    op29_body.extend_from_slice(&tid.to_be_bytes());
                    op29_body.extend_from_slice(&commit);
                    let op29 = WsMessage::Binary(
                        build_server_binary_frame(
                            12,
                            OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION,
                            &op29_body,
                        )
                        .into(),
                    );

                    let mut op30_body = Vec::with_capacity(2 + welcome.len());
                    op30_body.extend_from_slice(&tid.to_be_bytes());
                    op30_body.extend_from_slice(&welcome);
                    let op30 = WsMessage::Binary(
                        build_server_binary_frame(12, OP_DAVE_MLS_WELCOME, &op30_body).into(),
                    );

                    if let Ok(guard) = peers.lock() {
                        for p in guard.iter() {
                            if p.user_id == user_a {
                                let _ = p.tx.send(op29.clone());
                            } else {
                                let _ = p.tx.send(op30.clone());
                            }
                        }
                    }

                    // Wait for both Op 23 TransitionReady messages, then broadcast Op 22 ExecuteTransition!
                    let _ = ready_rx.recv().await;
                    let _ = ready_rx.recv().await;
                    let exec = WsMessage::Text(
                        json!({
                            "op": OP_DAVE_EXECUTE_TRANSITION,
                            "seq": 13,
                            "d": { "transition_id": tid }
                        })
                        .to_string()
                        .into(),
                    );
                    if let Ok(guard) = peers.lock() {
                        for p in guard.iter() {
                            let _ = p.tx.send(exec.clone());
                        }
                    }
                }
            });

            // Accept 2 WebSocket peers.
            for conn_idx in 0..2u32 {
                let Ok((stream, _)) = ws_listener.accept().await else {
                    break;
                };
                let peers = Arc::clone(&peers);
                let observed_server_ids = Arc::clone(&observed_server_ids);
                let kp_tx = kp_notify_tx.clone();
                let cw_tx = cw_notify_tx.clone();
                let ready_tx = ready_tx.clone();
                tokio::spawn(async move {
                    let ws = tokio_tungstenite::accept_async(stream)
                        .await
                        .expect("accept ws");
                    let (mut sink, mut stream) = ws.split();
                    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();

                    // Forward outbound queue to WebSocket sink.
                    let writer = tokio::spawn(async move {
                        while let Some(msg) = out_rx.recv().await {
                            if sink.send(msg).await.is_err() {
                                break;
                            }
                        }
                    });

                    // Send Op 8 HELLO.
                    let _ = out_tx.send(WsMessage::Text(
                        json!({
                            "op": VOICE_OP_HELLO,
                            "d": { "heartbeat_interval": 250, "v": 8 }
                        })
                        .to_string()
                        .into(),
                    ));

                    let assigned_ssrc = 1000 + conn_idx;
                    let mut my_uid = Snowflake(0);

                    while let Some(Ok(msg)) = stream.next().await {
                        match msg {
                            WsMessage::Text(txt) => {
                                let v: Value = serde_json::from_str(txt.as_str()).expect("json");
                                let op = v["op"].as_u64().unwrap_or(999) as u8;
                                match op {
                                    VOICE_OP_IDENTIFY => {
                                        let sid = v["d"]["server_id"].as_str().unwrap_or("").to_string();
                                        observed_server_ids.lock().expect("lock").push(sid);
                                        my_uid = serde_json::from_value(v["d"]["user_id"].clone())
                                            .expect("uid");
                                        peers.lock().expect("lock").push(ConnectedPeer {
                                            user_id: my_uid,
                                            ssrc: assigned_ssrc,
                                            tx: out_tx.clone(),
                                            udp_addr: None,
                                            key_package: None,
                                        });
                                        // Send Op 2 READY.
                                        let _ = out_tx.send(WsMessage::Text(
                                            json!({
                                                "op": VOICE_OP_READY,
                                                "seq": 1,
                                                "d": {
                                                    "ssrc": assigned_ssrc,
                                                    "ip": sfu_udp_addr.ip().to_string(),
                                                    "port": sfu_udp_addr.port(),
                                                    "modes": ["aead_xchacha20_poly1305_rtpsize", "aead_aes256_gcm_rtpsize"]
                                                }
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                    }
                                    VOICE_OP_SELECT_PROTOCOL => {
                                        let mode = v["d"]["data"]["mode"]
                                            .as_str()
                                            .unwrap_or("aead_xchacha20_poly1305_rtpsize");
                                        // Send Op 11 CLIENTS_CONNECT with both users so DAVE roster validates.
                                        let _ = out_tx.send(WsMessage::Text(
                                            json!({
                                                "op": VOICE_OP_CLIENTS_CONNECT,
                                                "seq": 2,
                                                "d": { "user_ids": [user_a.to_string(), user_b.to_string()] }
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                        // Send Op 4 SESSION_DESCRIPTION with dave_protocol_version = 1.
                                        let _ = out_tx.send(WsMessage::Text(
                                            json!({
                                                "op": VOICE_OP_SESSION_DESCRIPTION,
                                                "seq": 3,
                                                "d": {
                                                    "mode": mode,
                                                    "secret_key": secret_key.to_vec(),
                                                    "dave_protocol_version": 1
                                                }
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                    }
                                    VOICE_OP_SPEAKING => {
                                        // Relay SPEAKING with user_id + ssrc to all other peers.
                                        let spk_msg = WsMessage::Text(
                                            json!({
                                                "op": VOICE_OP_SPEAKING,
                                                "seq": 4,
                                                "d": {
                                                    "user_id": my_uid.to_string(),
                                                    "ssrc": assigned_ssrc,
                                                    "speaking": v["d"]["speaking"]
                                                }
                                            })
                                            .to_string()
                                            .into(),
                                        );
                                        if let Ok(guard) = peers.lock() {
                                            for p in guard.iter() {
                                                if p.user_id != my_uid {
                                                    let _ = p.tx.send(spk_msg.clone());
                                                }
                                            }
                                        }
                                    }
                                    VOICE_OP_HEARTBEAT => {
                                        let nonce = v["d"]["t"].as_u64().unwrap_or(0);
                                        let _ = out_tx.send(WsMessage::Text(
                                            json!({
                                                "op": VOICE_OP_HEARTBEAT_ACK,
                                                "d": { "t": nonce }
                                            })
                                            .to_string()
                                            .into(),
                                        ));
                                    }
                                    OP_DAVE_TRANSITION_READY => {
                                        let tid = v["d"]["transition_id"].as_u64().unwrap_or(0) as u16;
                                        let _ = ready_tx.send(tid);
                                    }
                                    _ => {}
                                }
                            }
                            WsMessage::Binary(bin) => {
                                if bin.is_empty() {
                                    continue;
                                }
                                match bin[0] {
                                    OP_DAVE_MLS_KEY_PACKAGE => {
                                        let kp = bin[1..].to_vec();
                                        if let Ok(mut guard) = peers.lock()
                                            && let Some(p) =
                                                guard.iter_mut().find(|p| p.user_id == my_uid)
                                        {
                                            p.key_package = Some(kp.clone());
                                        }
                                        let _ = kp_tx.send((my_uid, kp));
                                    }
                                    OP_DAVE_MLS_COMMIT_WELCOME => {
                                        // Parse Commit (MLSMessage) + Welcome from concatenated TLS bytes.
                                        let body = &bin[1..];
                                        let (_commit_msg, rest) =
                                            MlsMessageIn::tls_deserialize_bytes(body)
                                                .expect("split commit from Op 28");
                                        let commit_len = body.len() - rest.len();
                                        let commit = body[..commit_len].to_vec();
                                        let welcome = if rest.is_empty() {
                                            None
                                        } else {
                                            Some(rest.to_vec())
                                        };
                                        let _ = cw_tx.send((commit, welcome));
                                    }
                                    _ => {}
                                }
                            }
                            WsMessage::Close(_) => break,
                            _ => {}
                        }
                    }
                    writer.abort();
                });
            }

            let _ = dave_orch.await;
        }
    });

    // Construct TWO real DiscordVoiceEngines joining a Group DM call (`guild_id: None`)!
    let hub_a = MockSignalingHub::new(ws_endpoint.clone(), user_a, "sess-a".to_string());
    let hub_b = MockSignalingHub::new(ws_endpoint.clone(), user_b, "sess-b".to_string());

    let engine_a = Arc::new(
        DiscordVoiceEngine::new(
            VoiceEngineConfig {
                audio_mode: AudioBackendMode::Headless,
                jitter_cushion_frames: 2,
                input_mode: InputMode::VoiceActivity {
                    threshold_db: -45.0,
                },
                ..VoiceEngineConfig::new(user_a, "SysOp")
            },
            Some(Arc::clone(&hub_a) as Arc<dyn VoiceSignalingBridge>),
        )
        .expect("engine A"),
    );
    let engine_b = Arc::new(
        DiscordVoiceEngine::new(
            VoiceEngineConfig {
                audio_mode: AudioBackendMode::Headless,
                jitter_cushion_frames: 2,
                ..VoiceEngineConfig::new(user_b, "Modem Wizard")
            },
            Some(Arc::clone(&hub_b) as Arc<dyn VoiceSignalingBridge>),
        )
        .expect("engine B"),
    );

    hub_a.attach_engine(Arc::clone(&engine_a));
    hub_b.attach_engine(Arc::clone(&engine_b));

    let mut events_b = engine_b.subscribe_voice_events();

    // Join Group DM Call (`guild_id: None`)!
    engine_a
        .join_voice(None, dm_channel_id)
        .await
        .expect("A join Group DM call");
    engine_b
        .join_voice(None, dm_channel_id)
        .await
        .expect("B join Group DM call");

    // Wait for BOTH engines to reach Connected + DaveSessionState::Active!
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let da = engine_a.dave_state().await.expect("dave A");
        let db = engine_b.dave_state().await.expect("dave B");
        let sa = engine_a.status().await.expect("status A");
        let sb = engine_b.status().await.expect("status B");
        if matches!(da, DaveSessionState::Active { .. })
            && matches!(db, DaveSessionState::Active { .. })
            && sa.as_ref().is_some_and(|s| s.connection_state == VoiceConnectionState::Connected)
            && sb.as_ref().is_some_and(|s| s.connection_state == VoiceConnectionState::Connected)
        {
            assert_eq!(da, db, "Group DM peers must share identical DAVE Active state");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for DAVE Active handshake (A={da:?}, B={db:?})"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Verify Voice Gateway IDENTIFY used `server_id == dm_channel_id` for `guild_id: None`!
    {
        let sids = observed_server_ids.lock().expect("lock");
        assert_eq!(sids.len(), 2);
        for sid in sids.iter() {
            assert_eq!(sid, &dm_channel_id.to_string());
        }
    }

    // Verify pairwise SAS fingerprints match across both live DiscordVoiceEngine instances!
    let sas_a = engine_a.peer_verification_code(user_b).expect("SAS A->B");
    let sas_b = engine_b.peer_verification_code(user_a).expect("SAS B->A");
    assert_eq!(sas_a, sas_b);

    // Inject 10 frames of 440 Hz stereo audio into Peer A -> sent over UDP with DAVE + XChaCha20-Poly1305!
    for i in 0..10 {
        let pcm = generate_440hz_stereo_frame(i, 0.6).to_vec();
        engine_a.inject_mic_frame(pcm).await.expect("inject mic");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Confirm Peer B receives and decodes Peer A's audio and emits non-silent VoiceAudioLevels!
    let vu_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_peer_a_audio = false;
    while tokio::time::Instant::now() < vu_deadline {
        match tokio::time::timeout(Duration::from_millis(200), events_b.recv()).await {
            Ok(Ok(BackendEvent::VoiceAudioLevels { channel_id, levels }))
                if channel_id == dm_channel_id
                    && levels
                        .iter()
                        .any(|(uid, rms, spk)| *uid == user_a && *rms > -30.0 && *spk) =>
            {
                saw_peer_a_audio = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => {}
        }
    }
    assert!(
        saw_peer_a_audio,
        "Peer B must receive DAVE+AEAD encrypted 440 Hz audio from Peer A and emit active VU levels"
    );

    // Exercise volume clamping, mute, deafen, and clean leave.
    let updated_p = engine_b
        .set_participant_volume(user_a, 250)
        .await
        .expect("set volume");
    assert_eq!(updated_p.volume, 200);

    let muted = engine_a.set_self_mute(true).await.expect("mute");
    assert!(muted.self_mute);
    let deaf = engine_a.set_self_deaf(true).await.expect("deaf");
    assert!(deaf.self_deaf && deaf.self_mute);

    engine_a.leave_voice().await.expect("leave A");
    engine_b.leave_voice().await.expect("leave B");
    assert!(engine_a.status().await.expect("status A").is_none());
    assert!(engine_b.status().await.expect("status B").is_none());

    sfu_udp_task.abort();
    ws_task.abort();
}
