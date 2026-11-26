//! Discord Ogg/Opus voice-message decoder, waveform bucket generator, and play/pause/seek player.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use audiopus::coder::Decoder as OpusDecoder;
use audiopus::{Channels as OpusChannels, SampleRate as OpusSampleRate};
use bbs_core::events::BackendEvent;
use bbs_core::model::{Attachment, Snowflake, VoicePlaybackState};
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::audio_io::{AudioBackendMode, AudioIoHandle, PlaybackQueue};
use crate::error::{VoiceError, VoiceResult};
use crate::mixer::{CHANNELS, FRAME_SAMPLES_PER_CHANNEL, SAMPLE_RATE};

/// Maximum permitted raw Ogg/Opus voice-message payload size (`25 MiB`) to guard against OOM bombs.
pub const MAX_VOICE_MESSAGE_BYTES: usize = 25 * 1024 * 1024;
/// Maximum permitted decoded voice-message duration (`20 minutes` @ 48 kHz) to guard against OOM bombs.
pub const MAX_VOICE_MESSAGE_DURATION_SECS: usize = 20 * 60;
const MAX_VOICE_MESSAGE_FRAMES: usize = MAX_VOICE_MESSAGE_DURATION_SECS * (SAMPLE_RATE as usize);

const OGG_CAPTURE_PATTERN: &[u8; 4] = b"OggS";
const OGG_HEADER_MIN_LEN: usize = 27;
const OGG_FLAG_CONTINUED: u8 = 0x01;
const OGG_FLAG_BOS: u8 = 0x02;
const OGG_FLAG_EOS: u8 = 0x04;
const OPUS_HEAD_MAGIC: &[u8; 8] = b"OpusHead";
const OPUS_TAGS_MAGIC: &[u8; 8] = b"OpusTags";

/// Parsed RFC 7845 `OpusHead` header metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    /// Mapping version (RFC 7845 uses `1`).
    pub version: u8,
    /// Output channel count (`1` = mono, `2` = stereo).
    pub channels: u8,
    /// Number of 48 kHz samples per channel to discard from decoder startup.
    pub pre_skip: u16,
    /// Original input sample rate metadata (informational; Opus always decodes at 48 kHz).
    pub input_sample_rate: u32,
    /// Output gain in Q7.8 dB (`gain_db = output_gain_q7_8 / 256.0`).
    pub output_gain_q7_8: i16,
    /// Channel mapping family (`0` = mono/stereo RTP mapping).
    pub mapping_family: u8,
}

impl OpusHead {
    /// Parse an RFC 7845 `OpusHead` identification header packet.
    pub fn parse(packet: &[u8]) -> VoiceResult<Self> {
        if packet.len() < 19 || &packet[0..8] != OPUS_HEAD_MAGIC {
            return Err(VoiceError::Ogg(
                "missing or truncated RFC 7845 OpusHead packet".to_string(),
            ));
        }
        let version = packet[8];
        if (version >> 4) != 0 {
            return Err(VoiceError::Ogg(format!(
                "unsupported OpusHead major version {version}"
            )));
        }
        let channels = packet[9];
        if channels == 0 || channels > 2 {
            return Err(VoiceError::Ogg(format!(
                "unsupported OpusHead channel count {channels} (expected 1 or 2)"
            )));
        }
        let pre_skip = u16::from_le_bytes([packet[10], packet[11]]);
        let input_sample_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
        let output_gain_q7_8 = i16::from_le_bytes([packet[16], packet[17]]);
        let mapping_family = packet[18];

        Ok(Self {
            version,
            channels,
            pre_skip,
            input_sample_rate,
            output_gain_q7_8,
            mapping_family,
        })
    }

    /// Serialize an RFC 7845 `OpusHead` packet (used by test Ogg/Opus muxing).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(19);
        out.extend_from_slice(OPUS_HEAD_MAGIC);
        out.push(self.version);
        out.push(self.channels);
        out.extend_from_slice(&self.pre_skip.to_le_bytes());
        out.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        out.extend_from_slice(&self.output_gain_q7_8.to_le_bytes());
        out.push(self.mapping_family);
        out
    }
}

/// Decoded Ogg/Opus voice message containing 48 kHz interleaved stereo PCM and metadata.
#[derive(Debug, Clone)]
pub struct DecodedVoiceMessage {
    /// Parsed `OpusHead` metadata.
    pub head: OpusHead,
    /// Decoded 48 kHz interleaved stereo `f32` PCM (`[-1.0, 1.0]`).
    pub pcm_stereo: Vec<f32>,
    /// Total duration in seconds (`pcm_stereo.len() / 2 / 48000`).
    pub duration_secs: f32,
}

impl DecodedVoiceMessage {
    /// Total number of 48 kHz stereo sample frames.
    #[must_use]
    pub fn total_frames(&self) -> usize {
        self.pcm_stereo.len() / CHANNELS
    }
}

/// Bounds-checked RFC 3533 Ogg page parser extracting complete logical packets and final granule.
struct OggPacketStream {
    packets: Vec<Vec<u8>>,
    final_granule: u64,
}

fn parse_ogg_packets(data: &[u8]) -> VoiceResult<OggPacketStream> {
    let mut offset = 0usize;
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut current_packet: Vec<u8> = Vec::new();
    let mut final_granule = 0u64;
    let mut saw_any_page = false;

    while offset + OGG_HEADER_MIN_LEN <= data.len() {
        let page = &data[offset..];
        if &page[0..4] != OGG_CAPTURE_PATTERN {
            return Err(VoiceError::Ogg(format!(
                "invalid OggS capture pattern at byte offset {offset}"
            )));
        }
        let version = page[4];
        if version != 0 {
            return Err(VoiceError::Ogg(format!(
                "unsupported Ogg page version {version} at offset {offset}"
            )));
        }

        let header_type = page[5];
        let granule_raw = u64::from_le_bytes([
            page[6], page[7], page[8], page[9], page[10], page[11], page[12], page[13],
        ]);
        let segment_count = usize::from(page[26]);

        let seg_table_end = OGG_HEADER_MIN_LEN + segment_count;
        if seg_table_end > page.len() {
            return Err(VoiceError::Ogg(format!(
                "truncated Ogg segment table at offset {offset}"
            )));
        }

        let segments = &page[OGG_HEADER_MIN_LEN..seg_table_end];
        let body_len: usize = segments.iter().map(|&s| usize::from(s)).sum();
        let page_total_len = seg_table_end.saturating_add(body_len);
        if page_total_len > page.len() {
            return Err(VoiceError::Ogg(format!(
                "truncated Ogg page payload at offset {offset} (claims {body_len} bytes, {} available)",
                page.len().saturating_sub(seg_table_end)
            )));
        }

        saw_any_page = true;
        if granule_raw != u64::MAX && granule_raw > 0 {
            final_granule = granule_raw;
        }

        // If this page does NOT continue a prior packet, discard any incomplete trailing bytes.
        if (header_type & OGG_FLAG_CONTINUED) == 0 && !current_packet.is_empty() {
            current_packet.clear();
        }

        let body = &page[seg_table_end..page_total_len];
        let mut cursor = 0usize;
        for &seg_len in segments {
            let len = usize::from(seg_len);
            let next_cursor = cursor.saturating_add(len);
            if next_cursor > body.len() {
                return Err(VoiceError::Ogg(
                    "Ogg segment length overflowed page payload".to_string(),
                ));
            }
            current_packet.extend_from_slice(&body[cursor..next_cursor]);
            cursor = next_cursor;

            // Any lacing value < 255 terminates the current logical Ogg packet.
            if seg_len < 255 {
                packets.push(std::mem::take(&mut current_packet));
            }
        }

        offset = offset.saturating_add(page_total_len);
        if (header_type & OGG_FLAG_EOS) != 0 {
            break;
        }
    }

    if !saw_any_page {
        return Err(VoiceError::Ogg(
            "input contains no valid Ogg pages".to_string(),
        ));
    }

    Ok(OggPacketStream {
        packets,
        final_granule,
    })
}

/// Decode an Ogg/Opus voice message byte buffer into 48 kHz interleaved stereo `f32` PCM.
pub fn decode_ogg_opus(data: &[u8]) -> VoiceResult<DecodedVoiceMessage> {
    if data.len() > MAX_VOICE_MESSAGE_BYTES {
        return Err(VoiceError::Ogg(format!(
            "voice message exceeds maximum size ({} > {MAX_VOICE_MESSAGE_BYTES} bytes)",
            data.len()
        )));
    }
    let stream = parse_ogg_packets(data)?;
    if stream.packets.len() < 2 {
        return Err(VoiceError::Ogg(
            "Ogg/Opus stream missing OpusHead or OpusTags header packets".to_string(),
        ));
    }

    let head = OpusHead::parse(&stream.packets[0])?;
    if stream.packets[1].len() < 8 || &stream.packets[1][0..8] != OPUS_TAGS_MAGIC {
        return Err(VoiceError::Ogg(
            "Ogg/Opus stream missing RFC 7845 OpusTags packet".to_string(),
        ));
    }

    let opus_channels = if head.channels == 1 {
        OpusChannels::Mono
    } else {
        OpusChannels::Stereo
    };
    let mut decoder = OpusDecoder::new(OpusSampleRate::Hz48000, opus_channels)
        .map_err(|e| VoiceError::Opus(format!("Ogg OpusDecoder::new failed: {e}")))?;

    let gain_linear = if head.output_gain_q7_8 != 0 {
        10.0f32.powf(f32::from(head.output_gain_q7_8) / (256.0 * 20.0))
    } else {
        1.0
    };

    let ch_count = usize::from(head.channels);
    // Max Opus packet duration is 120 ms = 5760 samples/ch.
    let mut decode_buf = vec![0.0f32; FRAME_SAMPLES_PER_CHANNEL * 6 * ch_count];
    let mut pcm_stereo: Vec<f32> = Vec::new();
    let mut pre_skip_remaining = usize::from(head.pre_skip);

    for packet in &stream.packets[2..] {
        if packet.is_empty() {
            continue;
        }
        let decoded_per_ch = decoder
            .decode_float(Some(packet.as_slice()), &mut decode_buf[..], false)
            .map_err(|e| VoiceError::Opus(format!("Ogg Opus decode_float failed: {e}")))?;

        let start_frame = pre_skip_remaining.min(decoded_per_ch);
        pre_skip_remaining = pre_skip_remaining.saturating_sub(start_frame);

        let appended_frames = decoded_per_ch.saturating_sub(start_frame);
        let current_frames = pcm_stereo.len() / CHANNELS;
        if current_frames.saturating_add(appended_frames) > MAX_VOICE_MESSAGE_FRAMES {
            return Err(VoiceError::Ogg(format!(
                "voice message exceeds maximum decoded duration ({MAX_VOICE_MESSAGE_DURATION_SECS} s)"
            )));
        }

        for frame_idx in start_frame..decoded_per_ch {
            if ch_count == 1 {
                let s = (decode_buf[frame_idx] * gain_linear).clamp(-1.0, 1.0);
                pcm_stereo.push(s);
                pcm_stereo.push(s);
            } else {
                let base = frame_idx * 2;
                let l = (decode_buf[base] * gain_linear).clamp(-1.0, 1.0);
                let r = (decode_buf[base + 1] * gain_linear).clamp(-1.0, 1.0);
                pcm_stereo.push(l);
                pcm_stereo.push(r);
            }
        }
    }

    // Trim end padding if final_granule specifies an exact end position.
    if stream.final_granule > u64::from(head.pre_skip) {
        let expected_frames = (stream.final_granule - u64::from(head.pre_skip)) as usize;
        let expected_stereo_samples = expected_frames.saturating_mul(CHANNELS);
        if expected_stereo_samples > 0 && expected_stereo_samples < pcm_stereo.len() {
            pcm_stereo.truncate(expected_stereo_samples);
        }
    }

    let duration_secs = (pcm_stereo.len() / CHANNELS) as f32 / (SAMPLE_RATE as f32);
    Ok(DecodedVoiceMessage {
        head,
        pcm_stereo,
        duration_secs,
    })
}

/// Generate `bucket_count` normalized waveform amplitude bars (`0..=255`) from interleaved PCM.
#[must_use]
pub fn generate_waveform_buckets(
    pcm_interleaved: &[f32],
    channels: usize,
    bucket_count: usize,
) -> Vec<u8> {
    if bucket_count == 0 {
        return Vec::new();
    }
    let ch = channels.max(1);
    let total_frames = pcm_interleaved.len() / ch;
    if total_frames == 0 {
        return vec![0u8; bucket_count];
    }

    let mut rms_buckets = Vec::with_capacity(bucket_count);
    let mut peak_rms = 0.0f64;

    for b in 0..bucket_count {
        let start_frame = (b * total_frames) / bucket_count;
        let end_frame = (((b + 1) * total_frames) / bucket_count).max(start_frame + 1).min(total_frames);
        let mut sum_sq = 0.0f64;
        let mut count = 0usize;

        for f in start_frame..end_frame {
            let base = f * ch;
            for c in 0..ch {
                if let Some(&sample) = pcm_interleaved.get(base + c)
                    && sample.is_finite()
                {
                    let s = f64::from(sample);
                    sum_sq += s * s;
                    count += 1;
                }
            }
        }

        let rms = if count > 0 {
            (sum_sq / count as f64).sqrt()
        } else {
            0.0
        };
        peak_rms = peak_rms.max(rms);
        rms_buckets.push(rms);
    }

    if peak_rms <= 1e-6 {
        return vec![0u8; bucket_count];
    }

    // Normalize relative to peak with a mild floor so near-silence doesn't amplify noise to 255.
    let norm_denom = peak_rms.max(0.05);
    rms_buckets
        .into_iter()
        .map(|rms| ((rms / norm_denom).clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect()
}

/// Resample Discord's precomputed `VoiceMessageMeta::waveform` (`0..=255`) into `bucket_count` bars.
#[must_use]
pub fn resample_waveform(waveform: &[u8], bucket_count: usize) -> Vec<u8> {
    if bucket_count == 0 {
        return Vec::new();
    }
    if waveform.is_empty() {
        return vec![0u8; bucket_count];
    }
    if waveform.len() == bucket_count {
        return waveform.to_vec();
    }

    let mut out = Vec::with_capacity(bucket_count);
    let src_len = waveform.len();
    for b in 0..bucket_count {
        let start = (b * src_len) / bucket_count;
        let end = (((b + 1) * src_len) / bucket_count).max(start + 1).min(src_len);
        let slice = &waveform[start..end];
        let avg = slice.iter().map(|&v| u32::from(v)).sum::<u32>() / (slice.len() as u32);
        out.push(avg.min(255) as u8);
    }
    out
}

/// Compute the RFC 3533 Ogg CRC-32 checksum (polynomial `0x04C11DB7`, MSB-first, no reflection).
fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Append a single RFC 3533 Ogg page carrying `packet` into `out`.
fn write_ogg_page(
    out: &mut Vec<u8>,
    header_type: u8,
    granule_position: u64,
    serial: u32,
    page_seq: u32,
    packet: &[u8],
) {
    let mut segments = Vec::new();
    let mut remaining = packet.len();
    while remaining >= 255 {
        segments.push(255u8);
        remaining -= 255;
    }
    segments.push(remaining as u8);

    let start = out.len();
    out.extend_from_slice(OGG_CAPTURE_PATTERN);
    out.push(0); // stream_structure_version
    out.push(header_type);
    out.extend_from_slice(&granule_position.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&page_seq.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // CRC placeholder
    out.push(segments.len() as u8);
    out.extend_from_slice(&segments);
    out.extend_from_slice(packet);

    let crc = ogg_crc32(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
}

/// Mux raw Opus packets into a valid RFC 3533 / RFC 7845 Ogg/Opus byte stream.
///
/// Useful for voice-message recording export and offline integration tests.
#[must_use]
pub fn mux_ogg_opus(
    channels: u8,
    pre_skip: u16,
    frame_samples_per_packet: u64,
    opus_packets: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = Vec::new();
    let serial = 0x4242_5301u32;

    let head = OpusHead {
        version: 1,
        channels: channels.clamp(1, 2),
        pre_skip,
        input_sample_rate: SAMPLE_RATE,
        output_gain_q7_8: 0,
        mapping_family: 0,
    };
    write_ogg_page(&mut out, OGG_FLAG_BOS, 0, serial, 0, &head.to_bytes());

    let mut tags = Vec::new();
    tags.extend_from_slice(OPUS_TAGS_MAGIC);
    let vendor = b"bbscord";
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // user_comment_list_length
    write_ogg_page(&mut out, 0, 0, serial, 1, &tags);

    let mut granule = 0u64;
    for (idx, packet) in opus_packets.iter().enumerate() {
        granule = granule.saturating_add(frame_samples_per_packet);
        let flags = if idx + 1 == opus_packets.len() {
            OGG_FLAG_EOS
        } else {
            0
        };
        write_ogg_page(&mut out, flags, granule, serial, (idx + 2) as u32, packet);
    }
    out
}

#[derive(Debug)]
struct ActiveTrack {
    message_id: Snowflake,
    attachment_id: Snowflake,
    pcm_stereo: Vec<f32>,
    duration_secs: f32,
    frame_cursor: usize,
    playing: bool,
    waveform_64: Vec<u8>,
}

impl ActiveTrack {
    fn total_frames(&self) -> usize {
        self.pcm_stereo.len() / CHANNELS
    }

    fn playback_state(&self) -> VoicePlaybackState {
        let pos = (self.frame_cursor as f32) / (SAMPLE_RATE as f32);
        VoicePlaybackState {
            message_id: self.message_id,
            attachment_id: self.attachment_id,
            playing: self.playing,
            position_secs: pos.clamp(0.0, self.duration_secs),
            duration_secs: self.duration_secs,
        }
    }
}

/// Asynchronous inline Discord voice-message player supporting play/pause/seek/stop and waveforms.
pub struct VoiceMessagePlayer {
    track: Arc<Mutex<Option<ActiveTrack>>>,
    wake: Arc<Notify>,
    queue: Arc<PlaybackQueue>,
    audio_io: Arc<AudioIoHandle>,
    events: broadcast::Sender<BackendEvent>,
    task: Mutex<Option<(watch::Sender<bool>, JoinHandle<()>)>>,
}

impl std::fmt::Debug for VoiceMessagePlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceMessagePlayer")
            .field("state", &self.state())
            .finish()
    }
}

impl VoiceMessagePlayer {
    /// Construct a new [`VoiceMessagePlayer`] sharing an event broadcast bus and audio backend policy.
    pub fn new(
        events: broadcast::Sender<BackendEvent>,
        audio_mode: AudioBackendMode,
    ) -> VoiceResult<Self> {
        let audio_io = Arc::new(AudioIoHandle::spawn(audio_mode)?);
        Ok(Self {
            track: Arc::new(Mutex::new(None)),
            wake: Arc::new(Notify::new()),
            queue: Arc::new(PlaybackQueue::new()),
            audio_io,
            events,
            task: Mutex::new(None),
        })
    }

    /// Return the current [`VoicePlaybackState`], if a voice message is loaded.
    #[must_use]
    pub fn state(&self) -> Option<VoicePlaybackState> {
        let guard = self.track.lock().ok()?;
        guard.as_ref().map(ActiveTrack::playback_state)
    }

    /// Return normalized waveform bars (`0..=255`) for the active track at `bucket_count` width.
    #[must_use]
    pub fn waveform_buckets(&self, bucket_count: usize) -> Vec<u8> {
        let Ok(guard) = self.track.lock() else {
            return vec![0u8; bucket_count];
        };
        let Some(track) = guard.as_ref() else {
            return vec![0u8; bucket_count];
        };
        resample_waveform(&track.waveform_64, bucket_count)
    }

    /// Fetch (from `file://` or HTTP(S) CDN) and start playing a Discord voice-message attachment.
    pub async fn play_attachment(
        &self,
        message_id: Snowflake,
        attachment: &Attachment,
    ) -> VoiceResult<VoicePlaybackState> {
        let bytes = fetch_attachment_bytes(&attachment.url).await?;
        let fallback_duration = attachment
            .voice_message
            .as_ref()
            .map(|m| m.duration_secs);
        self.play_bytes(message_id, attachment.id, &bytes, fallback_duration)
            .await
    }

    /// Decode an in-memory Ogg/Opus voice message and start playback immediately.
    ///
    /// CPU-heavy Opus decoding and waveform generation run on `tokio::task::spawn_blocking`
    /// so Tokio async workers are never stalled.
    pub async fn play_bytes(
        &self,
        message_id: Snowflake,
        attachment_id: Snowflake,
        ogg_bytes: &[u8],
        fallback_duration_secs: Option<f32>,
    ) -> VoiceResult<VoicePlaybackState> {
        if ogg_bytes.len() > MAX_VOICE_MESSAGE_BYTES {
            return Err(VoiceError::Ogg(format!(
                "voice message exceeds maximum size ({} > {MAX_VOICE_MESSAGE_BYTES} bytes)",
                ogg_bytes.len()
            )));
        }

        self.stop_internal().await;

        let owned_bytes = ogg_bytes.to_vec();
        let (decoded, waveform_64) = tokio::task::spawn_blocking(move || -> VoiceResult<_> {
            let decoded = decode_ogg_opus(&owned_bytes)?;
            let waveform = generate_waveform_buckets(&decoded.pcm_stereo, CHANNELS, 64);
            Ok((decoded, waveform))
        })
        .await
        .map_err(|e| VoiceError::Internal(format!("voice message decode task failed: {e}")))??;

        let duration_secs = if decoded.duration_secs > 0.0 {
            decoded.duration_secs
        } else {
            fallback_duration_secs.unwrap_or(0.0).max(0.0)
        };

        let track = ActiveTrack {
            message_id,
            attachment_id,
            pcm_stereo: decoded.pcm_stereo,
            duration_secs,
            frame_cursor: 0,
            playing: true,
            waveform_64,
        };
        let initial_state = track.playback_state();

        {
            let mut guard = self
                .track
                .lock()
                .map_err(|_| VoiceError::Internal("player track lock poisoned".to_string()))?;
            *guard = Some(track);
        }

        self.queue.clear();
        let _ = self.audio_io.start_playback(Arc::clone(&self.queue)).await;
        let _ = self
            .events
            .send(BackendEvent::VoicePlaybackUpdated(initial_state.clone()));

        let (stop_tx, mut stop_rx) = watch::channel(false);
        let track_arc = Arc::clone(&self.track);
        let wake_arc = Arc::clone(&self.wake);
        let queue_arc = Arc::clone(&self.queue);
        let events = self.events.clone();

        let handle = tokio::spawn(async move {
            let mut tick_count = 0u32;

            loop {
                if *stop_rx.borrow() {
                    break;
                }

                // Park cleanly on `Notify` when paused or at EOF — zero idle 20 ms spinning,
                // and instant wake on `resume()` or `seek()` even after reaching EOF.
                let notified = wake_arc.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                let is_playing = {
                    let Ok(guard) = track_arc.lock() else {
                        break;
                    };
                    match guard.as_ref() {
                        Some(t) => t.playing,
                        None => break,
                    }
                };

                if !is_playing {
                    tokio::select! {
                        _ = stop_rx.changed() => {
                            if *stop_rx.borrow() {
                                break;
                            }
                        }
                        () = notified => {}
                    }
                    continue;
                }

                // Actively playing: stream 20 ms chunks until paused, EOF, or stop.
                let mut ticker = tokio::time::interval(Duration::from_millis(20));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        _ = stop_rx.changed() => {
                            if *stop_rx.borrow() {
                                return;
                            }
                        }
                        _ = ticker.tick() => {
                            let (chunk_to_push, maybe_emit, still_playing) = {
                                let Ok(mut guard) = track_arc.lock() else {
                                    return;
                                };
                                let Some(track) = guard.as_mut() else {
                                    return;
                                };
                                if !track.playing {
                                    (None, None, false)
                                } else {
                                    let total_frames = track.total_frames();
                                    if track.frame_cursor >= total_frames {
                                        track.playing = false;
                                        (None, Some(track.playback_state()), false)
                                    } else {
                                        let next_cursor = (track.frame_cursor + FRAME_SAMPLES_PER_CHANNEL)
                                            .min(total_frames);
                                        let start_sample = track.frame_cursor * CHANNELS;
                                        let end_sample = next_cursor * CHANNELS;
                                        let chunk = track.pcm_stereo[start_sample..end_sample].to_vec();
                                        track.frame_cursor = next_cursor;
                                        let done = track.frame_cursor >= total_frames;
                                        if done {
                                            track.playing = false;
                                        }
                                        tick_count = tick_count.wrapping_add(1);
                                        // Emit progress every 5 ticks (~100 ms) or upon completion.
                                        let emit = if done || tick_count.is_multiple_of(5) {
                                            Some(track.playback_state())
                                        } else {
                                            None
                                        };
                                        (Some(chunk), emit, !done)
                                    }
                                }
                            };

                            if let Some(chunk) = chunk_to_push {
                                queue_arc.push_samples(&chunk);
                            }
                            if let Some(state) = maybe_emit {
                                let _ = events.send(BackendEvent::VoicePlaybackUpdated(state));
                            }
                            if !still_playing {
                                // Break inner tick loop and return to parked Notify wait.
                                break;
                            }
                        }
                    }
                }
            }
        });

        if let Ok(mut guard) = self.task.lock() {
            *guard = Some((stop_tx, handle));
        }

        Ok(initial_state)
    }

    /// Pause playback while retaining the current playhead position.
    pub fn pause(&self) -> VoiceResult<VoicePlaybackState> {
        let state = {
            let mut guard = self
                .track
                .lock()
                .map_err(|_| VoiceError::Internal("player track lock poisoned".to_string()))?;
            let track = guard
                .as_mut()
                .ok_or_else(|| VoiceError::Ogg("no voice message currently loaded".to_string()))?;
            track.playing = false;
            track.playback_state()
        };
        self.queue.clear();
        let _ = self
            .events
            .send(BackendEvent::VoicePlaybackUpdated(state.clone()));
        Ok(state)
    }

    /// Resume playback from the current playhead position (restarts from `0.0` if at EOF).
    pub fn resume(&self) -> VoiceResult<VoicePlaybackState> {
        let state = {
            let mut guard = self
                .track
                .lock()
                .map_err(|_| VoiceError::Internal("player track lock poisoned".to_string()))?;
            let track = guard
                .as_mut()
                .ok_or_else(|| VoiceError::Ogg("no voice message currently loaded".to_string()))?;
            if track.frame_cursor >= track.total_frames() {
                track.frame_cursor = 0;
            }
            track.playing = true;
            track.playback_state()
        };
        self.wake.notify_one();
        let _ = self
            .events
            .send(BackendEvent::VoicePlaybackUpdated(state.clone()));
        Ok(state)
    }

    /// Toggle play/pause on the currently loaded voice message.
    pub fn toggle_pause(&self) -> VoiceResult<VoicePlaybackState> {
        let is_playing = self.state().is_some_and(|s| s.playing);
        if is_playing {
            self.pause()
        } else {
            self.resume()
        }
    }

    /// Seek the active voice message to `position_secs` (clamped safely to `0.0..=duration_secs`).
    pub fn seek(&self, position_secs: f32) -> VoiceResult<VoicePlaybackState> {
        let state = {
            let mut guard = self
                .track
                .lock()
                .map_err(|_| VoiceError::Internal("player track lock poisoned".to_string()))?;
            let track = guard
                .as_mut()
                .ok_or_else(|| VoiceError::Ogg("no voice message currently loaded".to_string()))?;
            let clamped = if position_secs.is_finite() {
                position_secs.clamp(0.0, track.duration_secs)
            } else {
                0.0
            };
            let target_frame = ((f64::from(clamped) * f64::from(SAMPLE_RATE)).round() as usize)
                .min(track.total_frames());
            track.frame_cursor = target_frame;
            track.playback_state()
        };
        self.queue.clear();
        self.wake.notify_one();
        let _ = self
            .events
            .send(BackendEvent::VoicePlaybackUpdated(state.clone()));
        Ok(state)
    }

    /// Stop active voice-message playback and release the playback audio device.
    pub async fn stop(&self) -> VoiceResult<()> {
        let stopped_state = {
            let mut guard = self
                .track
                .lock()
                .map_err(|_| VoiceError::Internal("player track lock poisoned".to_string()))?;
            guard.take().map(|mut t| {
                t.playing = false;
                t.playback_state()
            })
        };
        self.stop_internal().await;
        if let Some(state) = stopped_state {
            let _ = self.events.send(BackendEvent::VoicePlaybackUpdated(state));
        }
        Ok(())
    }

    async fn stop_internal(&self) {
        let prev = self.task.lock().ok().and_then(|mut g| g.take());
        if let Some((stop_tx, handle)) = prev {
            let _ = stop_tx.send(true);
            self.wake.notify_one();
            let _ = handle.await;
        }
        self.queue.clear();
        let _ = self.audio_io.stop_playback().await;
    }
}

impl Drop for VoiceMessagePlayer {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.task.lock()
            && let Some((stop_tx, handle)) = guard.take()
        {
            let _ = stop_tx.send(true);
            self.wake.notify_one();
            handle.abort();
        }
        self.queue.clear();
    }
}

async fn fetch_attachment_bytes(url: &str) -> VoiceResult<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        debug!("reading local voice message file");
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| VoiceError::Ogg(format!("failed to stat local voice message: {e}")))?;
        if meta.len() > MAX_VOICE_MESSAGE_BYTES as u64 {
            return Err(VoiceError::Ogg(format!(
                "local voice message exceeds maximum size ({} > {MAX_VOICE_MESSAGE_BYTES} bytes)",
                meta.len()
            )));
        }
        return tokio::fs::read(path)
            .await
            .map_err(|e| VoiceError::Ogg(format!("failed to read local voice message: {e}")));
    }

    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(VoiceError::Ogg(format!(
            "unsupported voice message URL scheme: {url}"
        )));
    }

    let client = wreq::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| VoiceError::Ogg(format!("HTTP client build failed: {e}")))?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| VoiceError::Ogg(format!("voice message download failed: {e}")))?;

    if !resp.status().is_success() {
        warn!("voice message HTTP status {}", resp.status());
        return Err(VoiceError::Ogg(format!(
            "voice message HTTP download returned status {}",
            resp.status()
        )));
    }

    if let Some(content_len) = resp.content_length()
        && content_len > MAX_VOICE_MESSAGE_BYTES as u64
    {
        return Err(VoiceError::Ogg(format!(
            "remote voice message Content-Length exceeds maximum ({content_len} > {MAX_VOICE_MESSAGE_BYTES} bytes)"
        )));
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| VoiceError::Ogg(format!("failed reading voice message chunk: {e}")))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_VOICE_MESSAGE_BYTES {
            return Err(VoiceError::Ogg(format!(
                "remote voice message body exceeds maximum size ({MAX_VOICE_MESSAGE_BYTES} bytes)"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiopus::coder::Encoder as OpusEncoder;
    use audiopus::Application;

    fn build_synthetic_ogg_opus_voice_message(duration_frames: usize) -> Vec<u8> {
        let encoder =
            OpusEncoder::new(OpusSampleRate::Hz48000, OpusChannels::Mono, Application::Voip)
                .expect("mono opus encoder");
        let lookahead = encoder.lookahead().unwrap_or(312) as u16;
        let mut packets = Vec::with_capacity(duration_frames);
        let mut out_buf = [0u8; 4000];

        for f in 0..duration_frames {
            let mut pcm = [0.0f32; FRAME_SAMPLES_PER_CHANNEL];
            let base = f * FRAME_SAMPLES_PER_CHANNEL;
            // Amplitude envelope peaking in the middle so waveform buckets vary clearly.
            let env = ((f as f32 / duration_frames.max(1) as f32) * std::f32::consts::PI).sin();
            for (i, sample) in pcm.iter_mut().enumerate() {
                let t = (base + i) as f32 / (SAMPLE_RATE as f32);
                *sample = 0.6 * env * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            }
            let len = encoder.encode_float(&pcm, &mut out_buf).expect("encode");
            packets.push(out_buf[..len].to_vec());
        }

        mux_ogg_opus(1, lookahead, FRAME_SAMPLES_PER_CHANNEL as u64, &packets)
    }

    #[test]
    fn ogg_opus_decode_and_waveform_buckets_succeed() {
        // 50 frames = 1.0 second of 440 Hz mono Ogg/Opus voice message.
        let ogg = build_synthetic_ogg_opus_voice_message(50);
        let decoded = decode_ogg_opus(&ogg).expect("decode ogg opus");
        assert_eq!(decoded.head.channels, 1);
        assert!(
            (decoded.duration_secs - 1.0).abs() < 0.05,
            "expected ~1.0s duration, got {}",
            decoded.duration_secs
        );

        let buckets = generate_waveform_buckets(&decoded.pcm_stereo, CHANNELS, 32);
        assert_eq!(buckets.len(), 32);
        let peak = buckets.iter().copied().max().unwrap_or(0);
        assert!(peak >= 200, "expected strong waveform peak, got {peak}");
        // Middle should be louder than first edge bucket because of sine envelope.
        assert!(buckets[16] > buckets[0]);

        let resampled = resample_waveform(&buckets, 16);
        assert_eq!(resampled.len(), 16);
    }

    #[test]
    fn truncated_ogg_page_does_not_panic() {
        // Matches discordo's TestPlayerSeek truncated page regression test.
        let mut page = vec![0u8; OGG_HEADER_MIN_LEN + 255];
        page[0..4].copy_from_slice(OGG_CAPTURE_PATTERN);
        page[26] = 255;
        for b in &mut page[OGG_HEADER_MIN_LEN..] {
            *b = 255;
        }
        assert!(decode_ogg_opus(&page).is_err());
        assert!(decode_ogg_opus(&[]).is_err());
        assert!(decode_ogg_opus(b"not-ogg").is_err());
    }

    #[tokio::test]
    async fn player_play_pause_seek_stop_lifecycle_headless() {
        let (tx, _rx) = broadcast::channel(32);
        let player = VoiceMessagePlayer::new(tx, AudioBackendMode::Headless).expect("player");
        let ogg = build_synthetic_ogg_opus_voice_message(50);

        let st = player
            .play_bytes(Snowflake(10), Snowflake(20), &ogg, None)
            .await
            .expect("play_bytes");
        assert!(st.playing);
        assert_eq!(st.message_id, Snowflake(10));
        assert_eq!(st.attachment_id, Snowflake(20));

        let paused = player.pause().expect("pause");
        assert!(!paused.playing);

        let sought = player.seek(0.5).expect("seek");
        assert!((sought.position_secs - 0.5).abs() < 0.02);

        let resumed = player.resume().expect("resume");
        assert!(resumed.playing);

        // Seeking beyond duration clamps cleanly without panic.
        let clamped = player.seek(999.0).expect("seek clamp");
        assert!((clamped.position_secs - clamped.duration_secs).abs() < 0.01);

        let bars = player.waveform_buckets(24);
        assert_eq!(bars.len(), 24);
        assert!(bars.iter().copied().max().unwrap_or(0) > 100);

        player.stop().await.expect("stop");
        assert!(player.state().is_none());
    }

    #[tokio::test]
    async fn player_replay_and_seek_after_eof() {
        let (tx, _rx) = broadcast::channel(64);
        let player = VoiceMessagePlayer::new(tx, AudioBackendMode::Headless).expect("player");
        // Short 4-frame (~80 ms) clip that reaches EOF quickly.
        let ogg = build_synthetic_ogg_opus_voice_message(4);

        player
            .play_bytes(Snowflake(30), Snowflake(40), &ogg, None)
            .await
            .expect("play");

        // Wait for EOF.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let st = player.state().expect("loaded");
            if !st.playing && (st.position_secs - st.duration_secs).abs() < 0.01 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for initial EOF: {st:?}"
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // Resume after EOF must restart from 0.0 and actually advance to EOF again (task stayed alive).
        let restarted = player.resume().expect("resume after EOF");
        assert!(restarted.playing);
        assert!(restarted.position_secs < 0.01);

        let deadline2 = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let st = player.state().expect("loaded");
            if !st.playing && (st.position_secs - st.duration_secs).abs() < 0.01 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline2,
                "timed out waiting for replay EOF after resume(): {st:?}"
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // Seek after EOF and resume must play from the sought position to EOF.
        let sought = player.seek(0.02).expect("seek after EOF");
        assert!((sought.position_secs - 0.02).abs() < 0.005);
        let resumed2 = player.resume().expect("resume after seek");
        assert!(resumed2.playing);

        let deadline3 = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let st = player.state().expect("loaded");
            if !st.playing && (st.position_secs - st.duration_secs).abs() < 0.01 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline3,
                "timed out waiting for EOF after seek()+resume(): {st:?}"
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        player.stop().await.expect("stop");
    }

    #[test]
    fn oversized_voice_message_is_rejected() {
        let huge = vec![0u8; MAX_VOICE_MESSAGE_BYTES + 1];
        assert!(decode_ogg_opus(&huge).is_err());
    }
}
