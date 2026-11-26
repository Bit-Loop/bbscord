//! Video codec descriptors, GStreamer initialization, and dynamic hardware/software encoder & decoder probing.

use std::sync::OnceLock;

use bbs_core::{CoreError, CoreResult, StreamQuality};
use davey::Codec as DaveCodec;
use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

static GST_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Initialize GStreamer once per process, caching any initialization error cleanly.
pub fn ensure_gst_init() -> CoreResult<()> {
    let result = GST_INIT.get_or_init(|| gst::init().map_err(|err| err.to_string()));
    match result {
        Ok(()) => Ok(()),
        Err(msg) => Err(CoreError::Video(format!(
            "failed to initialize GStreamer runtime: {msg}"
        ))),
    }
}

/// Supported Go Live video codecs for WebRTC SDP negotiation and DAVE E2EE transforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum VideoCodec {
    /// H.264 / AVC (preferred for hardware VA-API / NVENC acceleration and Discord Go Live parity).
    #[default]
    H264,
    /// VP8 (universal software fallback with wide WebRTC compatibility).
    Vp8,
    /// VP9 (high-efficiency screenshare fallback).
    Vp9,
}

/// Negotiable codec entry for Discord Stream RTC `SELECT_PROTOCOL` payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdpCodecSpec {
    /// Canonical uppercase codec name (`"H264"`, `"VP8"`, `"VP9"`).
    pub name: String,
    /// Media type (`"video"`).
    pub media_type: String,
    /// Primary RTP payload type (`96..=127`).
    pub payload_type: u8,
    /// Associated RTX retransmission payload type (`96..=127`).
    pub rtx_payload_type: u8,
    /// Codec priority order (1000, 2000, 3000...).
    pub priority: u32,
    /// Whether this codec is available for encoding on this host.
    pub encode: bool,
    /// Whether this codec is available for decoding on this host.
    pub decode: bool,
}

impl VideoCodec {
    /// Canonical Discord/WebRTC SDP encoding name.
    #[must_use]
    pub const fn sdp_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Vp8 => "VP8",
            Self::Vp9 => "VP9",
        }
    }

    /// Parse a Discord/WebRTC SDP codec name (case-insensitive).
    #[must_use]
    pub fn from_sdp_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "H264" | "AVC" | "X-H264" => Some(Self::H264),
            "VP8" | "X-VP8" => Some(Self::Vp8),
            "VP9" | "X-VP9" => Some(Self::Vp9),
            _ => None,
        }
    }

    /// Map to [`davey::Codec`] for codec-aware DAVE frame header preservation.
    #[must_use]
    pub const fn to_dave_codec(self) -> DaveCodec {
        match self {
            Self::H264 => DaveCodec::H264,
            Self::Vp8 => DaveCodec::VP8,
            Self::Vp9 => DaveCodec::VP9,
        }
    }

    /// Default Discord Go Live RTP payload type for this video codec.
    #[must_use]
    pub const fn default_payload_type(self) -> u8 {
        match self {
            Self::H264 => 101,
            Self::Vp8 => 103,
            Self::Vp9 => 105,
        }
    }

    /// Default Discord Go Live RTX retransmission payload type for this video codec.
    #[must_use]
    pub const fn default_rtx_payload_type(self) -> u8 {
        match self {
            Self::H264 => 102,
            Self::Vp8 => 104,
            Self::Vp9 => 106,
        }
    }

    /// GStreamer RTP payloader element factory name.
    #[must_use]
    pub const fn rtp_payloader_name(self) -> &'static str {
        match self {
            Self::H264 => "rtph264pay",
            Self::Vp8 => "rtpvp8pay",
            Self::Vp9 => "rtpvp9pay",
        }
    }

    /// GStreamer RTP depayloader element factory name.
    #[must_use]
    pub const fn rtp_depayloader_name(self) -> &'static str {
        match self {
            Self::H264 => "rtph264depay",
            Self::Vp8 => "rtpvp8depay",
            Self::Vp9 => "rtpvp9depay",
        }
    }

    /// GStreamer encoded elementary-stream caps string expected by DAVE's frame parser.
    ///
    /// For H.264, DAVE (`davey`) parses Annex-B 3/4-byte NAL start codes, so we enforce
    /// `stream-format=byte-stream,alignment=au`.
    #[must_use]
    pub const fn encoded_caps_str(self) -> &'static str {
        match self {
            Self::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
            Self::Vp8 => "video/x-vp8",
            Self::Vp9 => "video/x-vp9",
        }
    }

    /// Build the GStreamer `application/x-rtp` caps string for WebRTC RTP transport.
    #[must_use]
    pub fn rtp_caps_str(self, payload_type: u8) -> String {
        format!(
            "application/x-rtp,media=video,encoding-name={},payload={},clock-rate=90000",
            self.sdp_name(),
            payload_type
        )
    }

    /// Build the default ordered list of Discord Go Live video codecs for `SELECT_PROTOCOL`.
    #[must_use]
    pub fn negotiate_host_codec_specs() -> Vec<SdpCodecSpec> {
        let codecs = [Self::H264, Self::Vp8, Self::Vp9];
        let mut specs = Vec::with_capacity(codecs.len());
        for (idx, codec) in codecs.into_iter().enumerate() {
            let encode = probe_best_encoder(Some(codec))
                .map(|sel| sel.codec == codec)
                .unwrap_or(false);
            let decode = probe_best_decoder(codec).is_ok();
            if encode || decode {
                specs.push(SdpCodecSpec {
                    name: codec.sdp_name().to_string(),
                    media_type: "video".to_string(),
                    payload_type: codec.default_payload_type(),
                    rtx_payload_type: codec.default_rtx_payload_type(),
                    priority: ((idx as u32) + 1) * 1000,
                    encode,
                    decode,
                });
            }
        }
        specs
    }
}

/// Selected GStreamer video encoder resulting from dynamic host capability probing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSelection {
    /// Encoded video codec produced by this element.
    pub codec: VideoCodec,
    /// GStreamer element factory name (e.g., `"vah264enc"`, `"nvh264enc"`, `"x264enc"`, `"vp8enc"`).
    pub element_name: &'static str,
    /// `true` when backed by a GPU hardware encoder (VA-API / NVENC).
    pub hardware_accelerated: bool,
}

/// Selected GStreamer video decoder resulting from dynamic host capability probing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecoderSelection {
    /// Video codec accepted by this decoder.
    pub codec: VideoCodec,
    /// GStreamer element factory name (e.g., `"vah264dec"`, `"nvdec"`, `"avdec_h264"`, `"vp8dec"`).
    pub element_name: &'static str,
    /// `true` when backed by a GPU hardware decoder.
    pub hardware_accelerated: bool,
}

#[derive(Debug, Clone, Copy)]
struct ProbeCandidate {
    codec: VideoCodec,
    element_name: &'static str,
    hardware_accelerated: bool,
}

const ENCODER_CASCADE: &[ProbeCandidate] = &[
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "vah264enc",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "nvh264enc",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "vtenc_h264_hw",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "mfh264enc",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "x264enc",
        hardware_accelerated: false,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "openh264enc",
        hardware_accelerated: false,
    },
    ProbeCandidate {
        codec: VideoCodec::Vp8,
        element_name: "vp8enc",
        hardware_accelerated: false,
    },
    ProbeCandidate {
        codec: VideoCodec::Vp9,
        element_name: "vp9enc",
        hardware_accelerated: false,
    },
];

const H264_DECODER_CASCADE: &[ProbeCandidate] = &[
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "vah264dec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "nvdec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "nvh264dec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "vtdec_hw",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "d3d11h264dec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "avdec_h264",
        hardware_accelerated: false,
    },
    ProbeCandidate {
        codec: VideoCodec::H264,
        element_name: "openh264dec",
        hardware_accelerated: false,
    },
];

const VP8_DECODER_CASCADE: &[ProbeCandidate] = &[
    ProbeCandidate {
        codec: VideoCodec::Vp8,
        element_name: "vavp8dec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::Vp8,
        element_name: "nvdec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::Vp8,
        element_name: "vp8dec",
        hardware_accelerated: false,
    },
];

const VP9_DECODER_CASCADE: &[ProbeCandidate] = &[
    ProbeCandidate {
        codec: VideoCodec::Vp9,
        element_name: "vavp9dec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::Vp9,
        element_name: "nvdec",
        hardware_accelerated: true,
    },
    ProbeCandidate {
        codec: VideoCodec::Vp9,
        element_name: "vp9dec",
        hardware_accelerated: false,
    },
];

/// Probe whether a GStreamer element factory exists and can safely instantiate (and reach
/// `READY` for hardware elements so missing GPU/VA-API drivers degrade gracefully).
fn probe_element_usable(element_name: &str, verify_ready_state: bool) -> bool {
    let Some(factory) = gst::ElementFactory::find(element_name) else {
        return false;
    };
    let Ok(element) = factory.create().build() else {
        debug!(element = element_name, "element factory failed to instantiate");
        return false;
    };
    if verify_ready_state {
        let ready_ok = element.set_state(gst::State::Ready).is_ok();
        let _ = element.set_state(gst::State::Null);
        if !ready_ok {
            debug!(
                element = element_name,
                "hardware GStreamer element unavailable on host; falling back"
            );
            return false;
        }
    }
    true
}

/// Dynamically probe the best available video encoder on this host.
///
/// Order: `vah264enc` -> `nvh264enc` -> `x264enc` -> `openh264enc` -> `vp8enc` -> `vp9enc`.
/// When `preferred` is `Some(codec)`, matching encoders are tried first before falling back
/// to any other available encoder so broadcasting works even without GPU VA-API.
pub fn probe_best_encoder(preferred: Option<VideoCodec>) -> CoreResult<EncoderSelection> {
    ensure_gst_init()?;
    probe_encoder_with_policy(preferred, true)
}

/// Probe the best available software-only video encoder (useful for deterministic headless CI
/// pipelines where GPU contexts may not be desired).
pub fn probe_software_encoder(preferred: Option<VideoCodec>) -> CoreResult<EncoderSelection> {
    ensure_gst_init()?;
    probe_encoder_with_policy(preferred, false)
}

fn probe_encoder_with_policy(
    preferred: Option<VideoCodec>,
    allow_hardware: bool,
) -> CoreResult<EncoderSelection> {
    let candidates = ENCODER_CASCADE
        .iter()
        .filter(|cand| allow_hardware || !cand.hardware_accelerated);

    if let Some(pref) = preferred {
        for cand in candidates.clone().filter(|cand| cand.codec == pref) {
            if probe_element_usable(cand.element_name, cand.hardware_accelerated) {
                info!(
                    encoder = cand.element_name,
                    codec = ?cand.codec,
                    hardware = cand.hardware_accelerated,
                    "selected preferred GStreamer video encoder"
                );
                return Ok(EncoderSelection {
                    codec: cand.codec,
                    element_name: cand.element_name,
                    hardware_accelerated: cand.hardware_accelerated,
                });
            }
        }
    }

    for cand in candidates {
        if probe_element_usable(cand.element_name, cand.hardware_accelerated) {
            info!(
                encoder = cand.element_name,
                codec = ?cand.codec,
                hardware = cand.hardware_accelerated,
                "selected GStreamer video encoder"
            );
            return Ok(EncoderSelection {
                codec: cand.codec,
                element_name: cand.element_name,
                hardware_accelerated: cand.hardware_accelerated,
            });
        }
    }

    Err(CoreError::Video(
        "no supported GStreamer video encoder found (tried vah264enc, nvh264enc, x264enc, openh264enc, vp8enc, vp9enc)".to_string(),
    ))
}

/// Dynamically probe the best available video decoder for `codec` on this host.
///
/// Automatically prefers hardware decoders (`vah264dec` / `nvdec`) when operational and falls
/// back cleanly to software decoders (`avdec_h264` / `openh264dec` / `vp8dec` / `vp9dec`).
pub fn probe_best_decoder(codec: VideoCodec) -> CoreResult<DecoderSelection> {
    ensure_gst_init()?;
    probe_decoder_with_policy(codec, true)
}

/// Probe the best available software-only video decoder for `codec`.
pub fn probe_software_decoder(codec: VideoCodec) -> CoreResult<DecoderSelection> {
    ensure_gst_init()?;
    probe_decoder_with_policy(codec, false)
}

fn probe_decoder_with_policy(
    codec: VideoCodec,
    allow_hardware: bool,
) -> CoreResult<DecoderSelection> {
    let cascade = match codec {
        VideoCodec::H264 => H264_DECODER_CASCADE,
        VideoCodec::Vp8 => VP8_DECODER_CASCADE,
        VideoCodec::Vp9 => VP9_DECODER_CASCADE,
    };

    for cand in cascade
        .iter()
        .filter(|cand| allow_hardware || !cand.hardware_accelerated)
    {
        if probe_element_usable(cand.element_name, cand.hardware_accelerated) {
            info!(
                decoder = cand.element_name,
                codec = ?cand.codec,
                hardware = cand.hardware_accelerated,
                "selected GStreamer video decoder"
            );
            return Ok(DecoderSelection {
                codec: cand.codec,
                element_name: cand.element_name,
                hardware_accelerated: cand.hardware_accelerated,
            });
        }
    }

    Err(CoreError::Video(format!(
        "no supported GStreamer video decoder found for {codec:?}"
    )))
}

/// Instantiate and configure an encoder element for low-latency Go Live streaming.
pub fn create_configured_encoder(
    selection: &EncoderSelection,
    quality: StreamQuality,
) -> CoreResult<gst::Element> {
    ensure_gst_init()?;
    let encoder = gst::ElementFactory::make(selection.element_name)
        .name("video_encoder")
        .build()
        .map_err(|err| {
            CoreError::Video(format!(
                "failed to construct encoder `{}`: {err}",
                selection.element_name
            ))
        })?;

    let bitrate_kbps = quality.max_bitrate_kbps.clamp(250, 25_000);
    let keyframe_interval = u32::from(quality.fps.max(1)).saturating_mul(2);

    match selection.element_name {
        "x264enc" => {
            set_str_prop_if_present(&encoder, "tune", "zerolatency");
            set_str_prop_if_present(&encoder, "speed-preset", "ultrafast");
            if encoder.find_property("bitrate").is_some() {
                encoder.set_property("bitrate", bitrate_kbps);
            }
            if encoder.find_property("byte-stream").is_some() {
                encoder.set_property("byte-stream", true);
            }
            // Disable Access Unit Delimiters (NAL type 9) so RFC 6184 RTP packetizers
            // do not strip AUD NALs after DAVE computes the AEAD tag over the Access Unit.
            if encoder.find_property("aud").is_some() {
                encoder.set_property("aud", false);
            }
            if encoder.find_property("key-int-max").is_some() {
                encoder.set_property("key-int-max", keyframe_interval);
            }
        }
        "vah264enc" => {
            if encoder.find_property("bitrate").is_some() {
                encoder.set_property("bitrate", bitrate_kbps);
            }
            if encoder.find_property("key-int-max").is_some() {
                encoder.set_property("key-int-max", keyframe_interval);
            }
            set_str_prop_if_present(&encoder, "rate-control", "cbr");
        }
        "nvh264enc" => {
            if encoder.find_property("bitrate").is_some() {
                encoder.set_property("bitrate", bitrate_kbps);
            }
            if encoder.find_property("zerolatency").is_some() {
                encoder.set_property("zerolatency", true);
            }
            if encoder.find_property("gop-size").is_some() {
                encoder.set_property("gop-size", keyframe_interval as i32);
            }
            set_str_prop_if_present(&encoder, "preset", "low-latency-hq");
        }
        "openh264enc" => {
            if encoder.find_property("bitrate").is_some() {
                encoder.set_property("bitrate", bitrate_kbps.saturating_mul(1_000));
            }
            if encoder.find_property("gop-size").is_some() {
                encoder.set_property("gop-size", keyframe_interval);
            }
        }
        "vp8enc" | "vp9enc" => {
            if encoder.find_property("deadline").is_some() {
                encoder.set_property("deadline", 1i64);
            }
            if encoder.find_property("target-bitrate").is_some() {
                encoder.set_property("target-bitrate", (bitrate_kbps.saturating_mul(1_000)) as i32);
            }
            if encoder.find_property("keyframe-max-dist").is_some() {
                encoder.set_property("keyframe-max-dist", keyframe_interval as i32);
            }
            if encoder.find_property("cpu-used").is_some() {
                encoder.set_property("cpu-used", 6i32);
            }
            set_str_prop_if_present(&encoder, "error-resilient", "default");
        }
        other => {
            warn!(encoder = other, "using default settings for unknown encoder");
        }
    }

    Ok(encoder)
}

/// Configure an RTP payloader element (`rtph264pay`, `rtpvp8pay`, `rtpvp9pay`) for Discord WebRTC.
pub fn create_configured_payloader(codec: VideoCodec, payload_type: u8) -> CoreResult<gst::Element> {
    ensure_gst_init()?;
    let payloader = gst::ElementFactory::make(codec.rtp_payloader_name())
        .name("video_rtp_pay")
        .build()
        .map_err(|err| {
            CoreError::Video(format!(
                "failed to construct RTP payloader `{}`: {err}",
                codec.rtp_payloader_name()
            ))
        })?;

    if payloader.find_property("pt").is_some() {
        payloader.set_property("pt", u32::from(payload_type));
    }
    if payloader.find_property("mtu").is_some() {
        payloader.set_property("mtu", 1200u32);
    }
    if codec == VideoCodec::H264 {
        // Keep config-interval = 0 so rtph264pay never injects/mutates SPS/PPS NALs
        // after DAVE has already computed the AEAD tag over the Access Unit.
        if payloader.find_property("config-interval").is_some() {
            payloader.set_property("config-interval", 0i32);
        }
        set_str_prop_if_present(&payloader, "aggregate-mode", "none");
    }
    if codec == VideoCodec::Vp8 {
        set_str_prop_if_present(&payloader, "picture-id-mode", "15-bit");
    }

    Ok(payloader)
}

fn set_str_prop_if_present(element: &gst::Element, name: &str, value: &str) {
    if element.find_property(name).is_some() {
        element.set_property_from_str(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_sdp_mapping_and_dave_conversion() {
        assert_eq!(VideoCodec::from_sdp_name("h264"), Some(VideoCodec::H264));
        assert_eq!(VideoCodec::from_sdp_name("VP8"), Some(VideoCodec::Vp8));
        assert_eq!(VideoCodec::from_sdp_name("vp9"), Some(VideoCodec::Vp9));
        assert_eq!(VideoCodec::from_sdp_name("unknown"), None);
        assert_eq!(VideoCodec::H264.to_dave_codec(), DaveCodec::H264);
        assert_eq!(VideoCodec::Vp8.to_dave_codec(), DaveCodec::VP8);
        assert_eq!(VideoCodec::Vp9.to_dave_codec(), DaveCodec::VP9);
    }

    #[test]
    fn host_encoder_and_decoder_probes_succeed() {
        let enc = probe_best_encoder(None).expect("host should have at least one video encoder");
        let dec =
            probe_best_decoder(enc.codec).expect("host should decode the probed encoder codec");
        assert_eq!(enc.codec, dec.codec);

        let sw_enc =
            probe_software_encoder(Some(VideoCodec::Vp8)).expect("software encoder available");
        assert!(!sw_enc.hardware_accelerated);
        let sw_dec =
            probe_software_decoder(sw_enc.codec).expect("matching software decoder available");
        assert!(!sw_dec.hardware_accelerated);
    }
}
