//! Discord Voice Gateway v8, UDP/RTP AEAD transport, mandatory DAVE MLS E2EE,
//! real-time Opus audio pipeline/mixer, and Ogg/Opus voice-message playback for `bbscord`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audio_io;
pub mod dave;
pub mod error;
pub mod mixer;
pub mod player;
pub mod rtp;
pub mod session;

pub use audio_io::{AudioBackendKind, AudioBackendMode, AudioIoHandle, CaptureQueue, PlaybackQueue};
pub use dave::{
    DaveController, DaveInboundMessage, DaveOutboundMessage, DaveWireFrame,
    MAX_DAVE_PROTOCOL_VERSION, format_verification_code, parse_binary_dave_frame,
    parse_json_dave_frame,
};
pub use error::{VoiceError, VoiceResult};
pub use mixer::{
    AudioMixer, CHANNELS, DEFAULT_HIGH_PASS_CUTOFF_HZ, DEFAULT_JITTER_CUSHION_FRAMES,
    EncodedMicPacket, FRAME_DURATION_MS, FRAME_SAMPLES, FRAME_SAMPLES_PER_CHANNEL, HighPassFilter,
    InputMode, MAX_OPUS_PACKET_BYTES, MIN_RMS_DB, MicCaptureProcessor, MicFrameResult,
    OPUS_SILENCE_FRAME, SAMPLE_RATE, SpeakerPipeline, TRAILING_SILENCE_FRAMES, VAD_HANGOVER_FRAMES,
    compute_rms_db, smooth_rms_db, soft_clip_tanh,
};
pub use player::{
    DecodedVoiceMessage, MAX_VOICE_MESSAGE_BYTES, MAX_VOICE_MESSAGE_DURATION_SECS, OpusHead,
    VoiceMessagePlayer, decode_ogg_opus, generate_waveform_buckets, mux_ogg_opus,
    resample_waveform,
};
pub use rtp::{
    AEAD_NONCE_SUFFIX_LEN, AEAD_TAG_LEN, DISCOVERY_PACKET_LEN, DISCOVERY_PAYLOAD_LEN,
    DISCOVERY_TYPE_REQUEST, DISCOVERY_TYPE_RESPONSE, DecryptedRtpPacket, OPUS_TIMESTAMP_STEP,
    RTP_HEADER_MIN_LEN, RTP_PAYLOAD_TYPE_OPUS, RTP_VERSION, RtpHeader, RtpPacketizer,
    RtpTransportCipher, TransportEncryptionMode, build_ip_discovery_request, is_rtcp_packet,
    parse_ip_discovery_response,
};
pub use session::{
    CallUpdateEvent, DiscordVoiceEngine, SPEAKING_FLAG_MICROPHONE, SPEAKING_FLAG_PRIORITY,
    SPEAKING_FLAG_SOUNDSHARE, VoiceEngineConfig, VoiceGatewayCredentials, VoiceServerUpdateEvent,
    VoiceSignalingBridge, VoiceStateUpdateCommand, VoiceStateUpdateEvent, build_heartbeat_payload,
    build_identify_payload, build_resume_payload, build_select_protocol_payload,
    build_speaking_payload,
};
