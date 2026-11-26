//! Real-time Opus audio pipeline, VAD/PTT input processing, jitter buffer with PLC,
//! and soft-clipping multi-speaker output mixer.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use audiopus::coder::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use audiopus::{Application, Bitrate, Channels as OpusChannels, SampleRate as OpusSampleRate};
use bbs_core::model::{Snowflake, VoiceParticipant};
use davey::OPUS_SILENCE_PACKET;

use crate::error::{VoiceError, VoiceResult};

/// Discord voice sample rate (`48 kHz`).
pub const SAMPLE_RATE: u32 = 48_000;
/// Discord voice channel count (`2`, interleaved stereo).
pub const CHANNELS: usize = 2;
/// Nominal Opus voice frame duration (`20 ms`).
pub const FRAME_DURATION_MS: usize = 20;
/// Samples per channel in a single 20 ms frame (`48000 / 50 = 960`).
pub const FRAME_SAMPLES_PER_CHANNEL: usize = (SAMPLE_RATE as usize) / 50;
/// Total interleaved samples in a stereo 20 ms frame (`960 * 2 = 1920`).
pub const FRAME_SAMPLES: usize = FRAME_SAMPLES_PER_CHANNEL * CHANNELS;
/// Standard 3-byte Opus silence frame (`[0xF8, 0xFF, 0xFE]`), matching Discord & `discordo`.
pub const OPUS_SILENCE_FRAME: [u8; 3] = OPUS_SILENCE_PACKET;
/// Maximum encoded Opus packet buffer size in bytes.
pub const MAX_OPUS_PACKET_BYTES: usize = 4_000;
/// Number of 20 ms frames kept open after the last loud frame in VAD mode (`300 ms`), matching `discordo`.
pub const VAD_HANGOVER_FRAMES: usize = 15;
/// Number of Opus silence frames emitted after speech stops so remote decoders flush cleanly.
pub const TRAILING_SILENCE_FRAMES: usize = 5;
/// Minimum dBFS floor mapped to silence (`-60.0 dBFS`).
pub const MIN_RMS_DB: f32 = VoiceParticipant::MIN_RMS_DB;
/// Default jitter buffer prefill cushion in frames (`3 * 20 ms = 60 ms`).
pub const DEFAULT_JITTER_CUSHION_FRAMES: usize = 3;
/// Maximum jitter buffer depth before dropping oldest frames (`10 * 20 ms = 200 ms`).
pub const MAX_JITTER_BUFFER_FRAMES: usize = 10;
/// Maximum consecutive missing packets concealed via Opus PLC (`3 * 20 ms = 60 ms`).
pub const MAX_CONSECUTIVE_PLC_FRAMES: usize = 3;
/// Number of consecutive silent 20 ms mixer ticks (`500 ms`) after which a drained
/// [`SpeakerPipeline`] resets its RTP sequence tracking so peer reconnects never stall.
pub const SPEAKER_IDLE_RESET_FRAMES: usize = 25;
/// Wall-clock idle threshold (`500 ms`) resetting sequence tracking on the next talkspurt.
const SPEAKER_IDLE_RESET_DURATION: Duration = Duration::from_millis(500);
/// Maximum allowed per-user or master volume percentage (`200%`).
pub const MAX_VOLUME_PERCENT: u8 = VoiceParticipant::MAX_VOLUME;
/// Default high-pass DC / rumble filter cutoff frequency (`80 Hz`).
pub const DEFAULT_HIGH_PASS_CUTOFF_HZ: f32 = 80.0;

/// Compute RMS amplitude in dBFS (`-60.0..=0.0`) for an interleaved `f32` PCM buffer.
#[must_use]
pub fn compute_rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return MIN_RMS_DB;
    }
    let mut sum_sq = 0.0f64;
    for &sample in samples {
        if sample.is_finite() {
            let s = f64::from(sample);
            sum_sq += s * s;
        }
    }
    let rms = (sum_sq / (samples.len() as f64)).sqrt();
    if rms <= 1e-6 {
        return MIN_RMS_DB;
    }
    let db = 20.0 * rms.log10();
    (db as f32).clamp(MIN_RMS_DB, 0.0)
}

/// Smoothly update an exponentially averaged dBFS meter value (fast attack, smooth release).
#[must_use]
pub fn smooth_rms_db(previous_db: f32, current_db: f32) -> f32 {
    let alpha = if current_db > previous_db { 0.60 } else { 0.25 };
    (previous_db + alpha * (current_db - previous_db)).clamp(MIN_RMS_DB, 0.0)
}

/// Apply analog-style `tanh` soft-clipping saturation, keeping output strictly bounded in `[-1.0, 1.0]`.
#[must_use]
pub fn soft_clip_tanh(sample: f32) -> f32 {
    if !sample.is_finite() {
        return 0.0;
    }
    sample.tanh().clamp(-1.0, 1.0)
}

/// First-order stereo RC high-pass filter removing DC offset and low-frequency desk/HVAC rumble.
#[derive(Debug, Clone)]
pub struct HighPassFilter {
    alpha: f32,
    prev_in: [f32; CHANNELS],
    prev_out: [f32; CHANNELS],
}

impl HighPassFilter {
    /// Create a high-pass filter with cutoff `cutoff_hz` at `sample_rate`.
    #[must_use]
    pub fn new(cutoff_hz: f32, sample_rate: u32) -> Self {
        let fs = (sample_rate.max(1)) as f32;
        let fc = cutoff_hz.clamp(1.0, fs * 0.45);
        let rc = 1.0 / (2.0 * std::f32::consts::PI * fc);
        let dt = 1.0 / fs;
        let alpha = rc / (rc + dt);
        Self {
            alpha,
            prev_in: [0.0; CHANNELS],
            prev_out: [0.0; CHANNELS],
        }
    }

    /// Reset internal filter state (e.g. after mute/device switch).
    pub fn reset(&mut self) {
        self.prev_in = [0.0; CHANNELS];
        self.prev_out = [0.0; CHANNELS];
    }

    /// Filter an interleaved stereo PCM frame in place.
    pub fn process_interleaved_stereo(&mut self, pcm: &mut [f32]) {
        for frame in pcm.as_chunks_mut::<CHANNELS>().0 {
            for (ch, slot) in frame.iter_mut().enumerate() {
                let x = if slot.is_finite() { *slot } else { 0.0 };
                let y = self.alpha * (self.prev_out[ch] + x - self.prev_in[ch]);
                self.prev_in[ch] = x;
                self.prev_out[ch] = y;
                *slot = y;
            }
        }
    }
}

/// Microphone transmission gating mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// RMS/dBFS Voice Activity Detection with 15-frame hangover + 5 trailing Opus silence frames.
    VoiceActivity {
        /// Activation threshold in dBFS (e.g. `-42.0`).
        threshold_db: f32,
    },
    /// Explicit Push-to-Talk gated by `ptt_active` state + 5 trailing Opus silence frames on release.
    PushToTalk,
}

impl Default for InputMode {
    fn default() -> Self {
        Self::VoiceActivity {
            threshold_db: -42.0,
        }
    }
}

/// Encoded microphone transmission decision for a single 20 ms PCM capture frame.
#[derive(Debug, Clone, PartialEq)]
pub enum EncodedMicPacket {
    /// Active Opus-encoded audio packet ready for DAVE + RTP encryption.
    Audio(Vec<u8>),
    /// Explicit 3-byte Opus silence packet (`[0xF8, 0xFF, 0xFE]`) emitted on speech tail.
    Silence(Vec<u8>),
    /// Gate closed; no packet should be transmitted over UDP.
    Suppressed,
}

/// Output of [`MicCaptureProcessor::process_frame`].
#[derive(Debug, Clone, PartialEq)]
pub struct MicFrameResult {
    /// Encoded packet or suppression status.
    pub packet: EncodedMicPacket,
    /// Instantaneous post-filter RMS level in dBFS (`-60.0..=0.0`).
    pub rms_db: f32,
    /// Smoothed RMS level in dBFS (`-60.0..=0.0`) for Braille VU meters.
    pub smoothed_rms_db: f32,
    /// Whether the local user is currently considered actively speaking.
    pub speaking: bool,
}

/// Real-time microphone DSP chain: high-pass filter -> RMS/VAD or PTT gate -> `audiopus` encoder.
pub struct MicCaptureProcessor {
    mode: InputMode,
    ptt_active: bool,
    high_pass_enabled: bool,
    hp_filter: HighPassFilter,
    encoder: OpusEncoder,
    quiet_frames: usize,
    smoothed_rms_db: f32,
    scratch_pcm: Vec<f32>,
    encode_buf: Vec<u8>,
}

impl std::fmt::Debug for MicCaptureProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicCaptureProcessor")
            .field("mode", &self.mode)
            .field("ptt_active", &self.ptt_active)
            .field("high_pass_enabled", &self.high_pass_enabled)
            .field("quiet_frames", &self.quiet_frames)
            .field("smoothed_rms_db", &self.smoothed_rms_db)
            .finish()
    }
}

impl MicCaptureProcessor {
    /// Construct a new 48 kHz stereo VoIP microphone capture processor.
    pub fn new(mode: InputMode) -> VoiceResult<Self> {
        let mut encoder = OpusEncoder::new(
            OpusSampleRate::Hz48000,
            OpusChannels::Stereo,
            Application::Voip,
        )
        .map_err(|e| VoiceError::Opus(format!("OpusEncoder::new failed: {e}")))?;

        let _ = encoder.set_bitrate(Bitrate::BitsPerSecond(64_000));
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(5);

        Ok(Self {
            mode,
            ptt_active: false,
            high_pass_enabled: true,
            hp_filter: HighPassFilter::new(DEFAULT_HIGH_PASS_CUTOFF_HZ, SAMPLE_RATE),
            encoder,
            quiet_frames: VAD_HANGOVER_FRAMES + TRAILING_SILENCE_FRAMES,
            smoothed_rms_db: MIN_RMS_DB,
            scratch_pcm: vec![0.0; FRAME_SAMPLES],
            encode_buf: vec![0u8; MAX_OPUS_PACKET_BYTES],
        })
    }

    /// Return the active input gating mode.
    #[must_use]
    pub const fn input_mode(&self) -> InputMode {
        self.mode
    }

    /// Switch between VAD and Push-to-Talk modes.
    pub fn set_input_mode(&mut self, mode: InputMode) {
        self.mode = mode;
    }

    /// Update Push-to-Talk key state (`true` = transmitting, `false` = released).
    pub fn set_ptt_active(&mut self, active: bool) {
        self.ptt_active = active;
    }

    /// Return whether Push-to-Talk is currently pressed.
    #[must_use]
    pub const fn ptt_active(&self) -> bool {
        self.ptt_active
    }

    /// Enable or disable the 80 Hz DC/rumble high-pass filter.
    pub fn set_high_pass_enabled(&mut self, enabled: bool) {
        self.high_pass_enabled = enabled;
        if !enabled {
            self.hp_filter.reset();
        }
    }

    /// Return the latest smoothed microphone RMS level in dBFS.
    #[must_use]
    pub const fn smoothed_rms_db(&self) -> f32 {
        self.smoothed_rms_db
    }

    /// Emit trailing Opus silence frames and reset the gate when muting mid-speech.
    pub fn flush_on_mute(&mut self) -> Vec<Vec<u8>> {
        let was_active = match self.mode {
            InputMode::VoiceActivity { .. } => self.quiet_frames < VAD_HANGOVER_FRAMES,
            InputMode::PushToTalk => self.quiet_frames == 0,
        };
        self.quiet_frames = VAD_HANGOVER_FRAMES + TRAILING_SILENCE_FRAMES;
        self.smoothed_rms_db = MIN_RMS_DB;
        self.hp_filter.reset();
        if was_active {
            (0..TRAILING_SILENCE_FRAMES)
                .map(|_| OPUS_SILENCE_FRAME.to_vec())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Process one 20 ms stereo PCM capture frame (`1920` interleaved `f32` samples).
    pub fn process_frame(&mut self, pcm_interleaved: &[f32]) -> VoiceResult<MicFrameResult> {
        if pcm_interleaved.len() != FRAME_SAMPLES {
            return Err(VoiceError::Opus(format!(
                "expected {FRAME_SAMPLES} interleaved stereo samples (20 ms @ 48 kHz), got {}",
                pcm_interleaved.len()
            )));
        }

        self.scratch_pcm.copy_from_slice(pcm_interleaved);
        if self.high_pass_enabled {
            self.hp_filter
                .process_interleaved_stereo(&mut self.scratch_pcm);
        }

        let rms_db = compute_rms_db(&self.scratch_pcm);
        self.smoothed_rms_db = smooth_rms_db(self.smoothed_rms_db, rms_db);

        let (should_encode, should_send_silence, speaking) = match self.mode {
            InputMode::VoiceActivity { threshold_db } => {
                if rms_db >= threshold_db {
                    self.quiet_frames = 0;
                } else {
                    self.quiet_frames = self.quiet_frames.saturating_add(1);
                }

                if self.quiet_frames < VAD_HANGOVER_FRAMES {
                    (true, false, true)
                } else if self.quiet_frames < VAD_HANGOVER_FRAMES + TRAILING_SILENCE_FRAMES {
                    (false, true, false)
                } else {
                    (false, false, false)
                }
            }
            InputMode::PushToTalk => {
                if self.ptt_active {
                    self.quiet_frames = 0;
                    (true, false, true)
                } else if self.quiet_frames < TRAILING_SILENCE_FRAMES {
                    self.quiet_frames = self.quiet_frames.saturating_add(1);
                    (false, true, false)
                } else {
                    self.quiet_frames = VAD_HANGOVER_FRAMES + TRAILING_SILENCE_FRAMES;
                    (false, false, false)
                }
            }
        };

        let packet = if should_encode {
            let encoded_len = self
                .encoder
                .encode_float(&self.scratch_pcm, &mut self.encode_buf)
                .map_err(|e| VoiceError::Opus(format!("Opus encode_float failed: {e}")))?;
            EncodedMicPacket::Audio(self.encode_buf[..encoded_len].to_vec())
        } else if should_send_silence {
            EncodedMicPacket::Silence(OPUS_SILENCE_FRAME.to_vec())
        } else {
            EncodedMicPacket::Suppressed
        };

        Ok(MicFrameResult {
            packet,
            rms_db,
            smoothed_rms_db: self.smoothed_rms_db,
            speaking,
        })
    }
}

/// Sequence-ordered per-SSRC Opus decoder with jitter buffering and Packet Loss Concealment (PLC).
pub struct SpeakerPipeline {
    ssrc: u32,
    user_id: Option<Snowflake>,
    decoder: OpusDecoder,
    pending: BTreeMap<u64, Vec<u8>>,
    last_rx_seq: Option<u16>,
    seq_cycles: u64,
    next_play_seq: Option<u64>,
    jitter_cushion: usize,
    playing: bool,
    consecutive_plc: usize,
    idle_frames: usize,
    last_packet_at: Option<Instant>,
    packets_received: u64,
    packets_lost: u64,
    smoothed_rms_db: f32,
    speaking: bool,
    /// Scratch buffer supporting up to 120 ms Opus packets (`6 * 1920` samples).
    decode_scratch: Vec<f32>,
}

impl std::fmt::Debug for SpeakerPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakerPipeline")
            .field("ssrc", &self.ssrc)
            .field("user_id", &self.user_id)
            .field("pending_packets", &self.pending.len())
            .field("playing", &self.playing)
            .field("smoothed_rms_db", &self.smoothed_rms_db)
            .field("speaking", &self.speaking)
            .finish()
    }
}

impl SpeakerPipeline {
    /// Create a per-SSRC decoder pipeline with the specified jitter cushion (in 20 ms frames).
    pub fn new(ssrc: u32, user_id: Option<Snowflake>, jitter_cushion: usize) -> VoiceResult<Self> {
        let decoder = OpusDecoder::new(OpusSampleRate::Hz48000, OpusChannels::Stereo)
            .map_err(|e| VoiceError::Opus(format!("OpusDecoder::new failed: {e}")))?;
        Ok(Self {
            ssrc,
            user_id,
            decoder,
            pending: BTreeMap::new(),
            last_rx_seq: None,
            seq_cycles: 0,
            next_play_seq: None,
            jitter_cushion: jitter_cushion.clamp(1, MAX_JITTER_BUFFER_FRAMES),
            playing: false,
            consecutive_plc: 0,
            idle_frames: 0,
            last_packet_at: None,
            packets_received: 0,
            packets_lost: 0,
            smoothed_rms_db: MIN_RMS_DB,
            speaking: false,
            decode_scratch: vec![0.0; FRAME_SAMPLES * 6],
        })
    }

    /// Return this speaker's SSRC.
    #[must_use]
    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Return the bound Discord user Snowflake, if known.
    #[must_use]
    pub const fn user_id(&self) -> Option<Snowflake> {
        self.user_id
    }

    /// Associate or update the Discord user Snowflake mapped to this SSRC.
    pub fn set_user_id(&mut self, user_id: Snowflake) {
        self.user_id = Some(user_id);
    }

    /// Return the latest smoothed RMS level in dBFS.
    #[must_use]
    pub const fn smoothed_rms_db(&self) -> f32 {
        self.smoothed_rms_db
    }

    /// Return whether this speaker is currently producing speech audio.
    #[must_use]
    pub const fn speaking(&self) -> bool {
        self.speaking
    }

    /// Return `(packets_received, packets_lost)` counters for loss statistics.
    #[must_use]
    pub const fn packet_stats(&self) -> (u64, u64) {
        (self.packets_received, self.packets_lost)
    }

    /// Unwrap a 16-bit RTP sequence number into a monotonic 64-bit extended sequence number.
    fn unwrap_sequence(&mut self, seq: u16) -> u64 {
        let Some(prev) = self.last_rx_seq else {
            self.last_rx_seq = Some(seq);
            return u64::from(seq);
        };

        let delta = seq.wrapping_sub(prev) as i16;
        if delta >= 0 {
            if seq < prev {
                self.seq_cycles = self.seq_cycles.saturating_add(1);
            }
            self.last_rx_seq = Some(seq);
            (self.seq_cycles << 16) | u64::from(seq)
        } else {
            // Reordered packet behind `prev`: determine whether it belongs to previous cycle.
            let cycle = if seq > prev {
                self.seq_cycles.saturating_sub(1)
            } else {
                self.seq_cycles
            };
            (cycle << 16) | u64::from(seq)
        }
    }

    fn reset_sequence_tracking(&mut self) {
        self.last_rx_seq = None;
        self.seq_cycles = 0;
        self.next_play_seq = None;
        self.consecutive_plc = 0;
        self.idle_frames = 0;
    }

    /// Insert a decrypted Opus packet with RTP sequence number `seq` into the jitter buffer.
    pub fn push_packet(&mut self, seq: u16, opus_payload: Vec<u8>) {
        if opus_payload.is_empty() {
            return;
        }
        let now = Instant::now();
        if !self.playing
            && self.pending.is_empty()
            && self
                .last_packet_at
                .is_some_and(|last| now.saturating_duration_since(last) >= SPEAKER_IDLE_RESET_DURATION)
        {
            self.reset_sequence_tracking();
        }
        self.last_packet_at = Some(now);
        self.idle_frames = 0;

        let ext_seq = self.unwrap_sequence(seq);
        self.packets_received = self.packets_received.saturating_add(1);

        let play_seq = *self.next_play_seq.get_or_insert(ext_seq);
        if ext_seq < play_seq {
            if !self.playing && play_seq.saturating_sub(ext_seq) <= MAX_JITTER_BUFFER_FRAMES as u64 {
                // Early out-of-order packet arrived before jitter cushion began playing; rewind start.
                self.next_play_seq = Some(ext_seq);
            } else {
                // Arrived too late (already played or concealed).
                return;
            }
        }
        self.pending.entry(ext_seq).or_insert(opus_payload);

        if self.pending.len() > MAX_JITTER_BUFFER_FRAMES {
            // Drop oldest frames to prevent unbounded latency buildup.
            while self.pending.len() > self.jitter_cushion {
                self.pending.pop_first();
            }
            if let Some(&first_seq) = self.pending.keys().next() {
                self.next_play_seq = Some(first_seq);
            }
        }

        if self.pending.len() >= self.jitter_cushion {
            self.playing = true;
        }
    }

    /// Decode the next 20 ms stereo PCM frame (or conceal missing frames via Opus PLC).
    ///
    /// Returns `Ok(true)` if audio was written into `out_pcm`, or `Ok(false)` if the speaker is
    /// currently silent / waiting for the jitter cushion to fill.
    pub fn decode_next_frame(&mut self, out_pcm: &mut [f32; FRAME_SAMPLES]) -> VoiceResult<bool> {
        self.decode_next_frame_internal(out_pcm, false)
    }

    /// Like [`Self::decode_next_frame`], optionally flushing buffered packets even below the cushion.
    pub fn decode_next_frame_flushing(
        &mut self,
        out_pcm: &mut [f32; FRAME_SAMPLES],
    ) -> VoiceResult<bool> {
        self.decode_next_frame_internal(out_pcm, true)
    }

    fn decode_next_frame_internal(
        &mut self,
        out_pcm: &mut [f32; FRAME_SAMPLES],
        flush: bool,
    ) -> VoiceResult<bool> {
        if !self.playing {
            if flush && !self.pending.is_empty() {
                self.playing = true;
            } else {
                self.speaking = false;
                self.smoothed_rms_db = smooth_rms_db(self.smoothed_rms_db, MIN_RMS_DB);
                if self.pending.is_empty() && self.next_play_seq.is_some() {
                    self.idle_frames = self.idle_frames.saturating_add(1);
                    if self.idle_frames >= SPEAKER_IDLE_RESET_FRAMES {
                        self.reset_sequence_tracking();
                    }
                }
                return Ok(false);
            }
        }

        let Some(mut target_seq) = self.next_play_seq else {
            self.playing = false;
            self.speaking = false;
            return Ok(false);
        };

        if let Some(packet) = self.pending.remove(&target_seq) {
            self.next_play_seq = Some(target_seq.saturating_add(1));
            self.consecutive_plc = 0;
            return self.decode_opus_into(Some(&packet), false, out_pcm);
        }

        // Target sequence is missing — check whether newer packets already arrived (loss/reorder).
        if let Some(&earliest_available) = self.pending.keys().next() {
            let gap = earliest_available.saturating_sub(target_seq);
            if gap > 0 && self.consecutive_plc < MAX_CONSECUTIVE_PLC_FRAMES {
                self.packets_lost = self.packets_lost.saturating_add(1);
                self.consecutive_plc = self.consecutive_plc.saturating_add(1);
                self.next_play_seq = Some(target_seq.saturating_add(1));

                // Prefer in-band FEC if the immediate next packet is already buffered; else PLC.
                if let Some(next_pkt) = self.pending.get(&target_seq.saturating_add(1)).cloned()
                    && self.decode_opus_into(Some(&next_pkt), true, out_pcm).is_ok()
                {
                    return Ok(true);
                }
                return self.decode_opus_into(None, false, out_pcm);
            }

            // Gap exceeds PLC limit: resynchronize to the earliest buffered packet cleanly.
            target_seq = earliest_available;
            if let Some(packet) = self.pending.remove(&target_seq) {
                self.next_play_seq = Some(target_seq.saturating_add(1));
                self.consecutive_plc = 0;
                return self.decode_opus_into(Some(&packet), false, out_pcm);
            }
        }

        // Jitter buffer drained cleanly.
        self.playing = false;
        self.speaking = false;
        self.consecutive_plc = 0;
        self.idle_frames = self.idle_frames.saturating_add(1);
        if self.idle_frames >= SPEAKER_IDLE_RESET_FRAMES {
            self.reset_sequence_tracking();
        }
        self.smoothed_rms_db = smooth_rms_db(self.smoothed_rms_db, MIN_RMS_DB);
        Ok(false)
    }

    fn decode_opus_into(
        &mut self,
        packet: Option<&[u8]>,
        fec: bool,
        out_pcm: &mut [f32; FRAME_SAMPLES],
    ) -> VoiceResult<bool> {
        let is_explicit_silence = packet == Some(OPUS_SILENCE_FRAME.as_slice());

        let decoded_per_ch = if let Some(pkt) = packet {
            if fec {
                self.decoder
                    .decode_float(Some(pkt), &mut self.decode_scratch[..FRAME_SAMPLES], true)
            } else {
                self.decoder
                    .decode_float(Some(pkt), &mut self.decode_scratch[..], false)
            }
        } else {
            self.decoder
                .decode_float(None::<&[u8]>, &mut self.decode_scratch[..FRAME_SAMPLES], false)
        }
        .map_err(|e| VoiceError::Opus(format!("Opus decode_float failed: {e}")))?;

        let sample_count = decoded_per_ch.saturating_mul(CHANNELS).min(FRAME_SAMPLES);
        out_pcm.fill(0.0);
        out_pcm[..sample_count].copy_from_slice(&self.decode_scratch[..sample_count]);

        if is_explicit_silence {
            self.speaking = false;
            self.smoothed_rms_db = MIN_RMS_DB;
            if self.pending.is_empty() {
                self.playing = false;
            }
        } else {
            let rms_db = compute_rms_db(out_pcm);
            self.smoothed_rms_db = smooth_rms_db(self.smoothed_rms_db, rms_db);
            self.speaking = self.smoothed_rms_db > -55.0;
        }
        Ok(true)
    }
}

/// Multi-speaker voice mixer applying `0..=200%` per-user gain, master gain, and `tanh` soft clipping.
#[derive(Debug)]
pub struct AudioMixer {
    speakers: HashMap<u32, SpeakerPipeline>,
    ssrc_to_user: HashMap<u32, Snowflake>,
    user_volumes: HashMap<Snowflake, u8>,
    master_volume: u8,
    deafened: bool,
    jitter_cushion: usize,
    local_user: Option<Snowflake>,
    local_mic_rms_db: f32,
    local_mic_speaking: bool,
    scratch_speaker: [f32; FRAME_SAMPLES],
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new(DEFAULT_JITTER_CUSHION_FRAMES)
    }
}

impl AudioMixer {
    /// Construct a new multi-speaker [`AudioMixer`] with `jitter_cushion` prefill frames.
    #[must_use]
    pub fn new(jitter_cushion: usize) -> Self {
        Self {
            speakers: HashMap::new(),
            ssrc_to_user: HashMap::new(),
            user_volumes: HashMap::new(),
            master_volume: 100,
            deafened: false,
            jitter_cushion: jitter_cushion.clamp(1, MAX_JITTER_BUFFER_FRAMES),
            local_user: None,
            local_mic_rms_db: MIN_RMS_DB,
            local_mic_speaking: false,
            scratch_speaker: [0.0; FRAME_SAMPLES],
        }
    }

    /// Set the local user Snowflake so local mic VU telemetry is included in level snapshots.
    pub fn set_local_user(&mut self, user_id: Snowflake) {
        self.local_user = Some(user_id);
    }

    /// Update the latest local microphone RMS level and speaking indicator.
    pub fn update_local_mic_level(&mut self, rms_db: f32, speaking: bool) {
        self.local_mic_rms_db = rms_db.clamp(MIN_RMS_DB, 0.0);
        self.local_mic_speaking = speaking;
    }

    /// Bind an RTP `ssrc` to a Discord `user_id` (from Voice Gateway `SPEAKING` Op 5).
    pub fn bind_ssrc(&mut self, ssrc: u32, user_id: Snowflake) {
        self.ssrc_to_user.insert(ssrc, user_id);
        if let Some(speaker) = self.speakers.get_mut(&ssrc) {
            speaker.set_user_id(user_id);
        }
    }

    /// Look up the user ID mapped to `ssrc`, if known.
    #[must_use]
    pub fn user_for_ssrc(&self, ssrc: u32) -> Option<Snowflake> {
        self.ssrc_to_user.get(&ssrc).copied()
    }

    /// Remove all speaker state associated with a disconnected user.
    pub fn remove_user(&mut self, user_id: Snowflake) {
        self.ssrc_to_user.retain(|_, &mut uid| uid != user_id);
        self.speakers
            .retain(|_, speaker| speaker.user_id() != Some(user_id));
    }

    /// Set per-user playback gain in percent (`0..=200`, where `100` is unity gain).
    pub fn set_user_volume(&mut self, user_id: Snowflake, volume: u8) -> u8 {
        let clamped = volume.min(MAX_VOLUME_PERCENT);
        self.user_volumes.insert(user_id, clamped);
        clamped
    }

    /// Query per-user gain percentage (`0..=200`, defaults to `100`).
    #[must_use]
    pub fn user_volume(&self, user_id: Snowflake) -> u8 {
        self.user_volumes.get(&user_id).copied().unwrap_or(100)
    }

    /// Set master output gain in percent (`0..=200`, where `100` is unity gain).
    pub fn set_master_volume(&mut self, volume: u8) -> u8 {
        self.master_volume = volume.min(MAX_VOLUME_PERCENT);
        self.master_volume
    }

    /// Query master output gain percentage (`0..=200`).
    #[must_use]
    pub const fn master_volume(&self) -> u8 {
        self.master_volume
    }

    /// Enable or disable deafened mode (silences mixed output while continuing to drain queues).
    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
    }

    /// Push a decrypted Opus packet for `ssrc` into its sequence-ordered jitter buffer.
    pub fn push_opus_packet(
        &mut self,
        ssrc: u32,
        user_id: Option<Snowflake>,
        seq: u16,
        opus_payload: Vec<u8>,
    ) -> VoiceResult<()> {
        let resolved_user = user_id.or_else(|| self.ssrc_to_user.get(&ssrc).copied());
        if let Some(uid) = resolved_user {
            self.ssrc_to_user.insert(ssrc, uid);
        }

        if let std::collections::hash_map::Entry::Vacant(entry) = self.speakers.entry(ssrc) {
            entry.insert(SpeakerPipeline::new(ssrc, resolved_user, self.jitter_cushion)?);
        }

        if let Some(speaker) = self.speakers.get_mut(&ssrc) {
            if let Some(uid) = resolved_user {
                speaker.set_user_id(uid);
            }
            speaker.push_packet(seq, opus_payload);
        }
        Ok(())
    }

    /// Mix one 20 ms stereo frame across all active speakers into `output` with `tanh` soft clipping.
    ///
    /// Returns the number of speakers that contributed audio to this frame.
    pub fn mix_frame(&mut self, output: &mut [f32; FRAME_SAMPLES]) -> usize {
        self.mix_frame_internal(output, false)
    }

    /// Like [`Self::mix_frame`], flushing any remaining frames below the jitter cushion.
    pub fn mix_frame_flushing(&mut self, output: &mut [f32; FRAME_SAMPLES]) -> usize {
        self.mix_frame_internal(output, true)
    }

    fn mix_frame_internal(&mut self, output: &mut [f32; FRAME_SAMPLES], flush: bool) -> usize {
        output.fill(0.0);
        let master_gain = f32::from(self.master_volume) / 100.0;
        let mut active_speakers = 0usize;

        for speaker in self.speakers.values_mut() {
            let decode_res = if flush {
                speaker.decode_next_frame_flushing(&mut self.scratch_speaker)
            } else {
                speaker.decode_next_frame(&mut self.scratch_speaker)
            };

            let Ok(true) = decode_res else {
                continue;
            };

            active_speakers += 1;
            if self.deafened {
                continue;
            }

            let user_vol = speaker
                .user_id()
                .and_then(|uid| self.user_volumes.get(&uid).copied())
                .unwrap_or(100);
            let gain = (f32::from(user_vol) / 100.0) * master_gain;

            for (dst, &src) in output.iter_mut().zip(self.scratch_speaker.iter()) {
                *dst += src * gain;
            }
        }

        if !self.deafened && active_speakers > 0 {
            for sample in output.iter_mut() {
                *sample = soft_clip_tanh(*sample);
            }
        }

        active_speakers
    }

    /// Collect real-time `(user_id, rms_db, speaking)` tuples for all participants + local mic.
    #[must_use]
    pub fn audio_levels_snapshot(&self) -> Vec<(Snowflake, f32, bool)> {
        let mut by_user: HashMap<Snowflake, (f32, bool)> = HashMap::new();

        if let Some(local_uid) = self.local_user {
            by_user.insert(local_uid, (self.local_mic_rms_db, self.local_mic_speaking));
        }

        for speaker in self.speakers.values() {
            if let Some(uid) = speaker.user_id() {
                let rms = speaker.smoothed_rms_db();
                let speaking = speaker.speaking();
                by_user
                    .entry(uid)
                    .and_modify(|(prev_rms, prev_spk)| {
                        *prev_rms = prev_rms.max(rms);
                        *prev_spk = *prev_spk || speaking;
                    })
                    .or_insert((rms, speaking));
            }
        }

        let mut levels: Vec<(Snowflake, f32, bool)> = by_user
            .into_iter()
            .map(|(uid, (rms, spk))| (uid, rms, spk))
            .collect();
        levels.sort_by_key(|(uid, _, _)| *uid);
        levels
    }

    /// Compute aggregate packet loss percentage (`0.0..=100.0`) across all speakers.
    #[must_use]
    pub fn packet_loss_pct(&self) -> f32 {
        let mut rx = 0u64;
        let mut lost = 0u64;
        for speaker in self.speakers.values() {
            let (r, l) = speaker.packet_stats();
            rx = rx.saturating_add(r);
            lost = lost.saturating_add(l);
        }
        let total = rx.saturating_add(lost);
        if total == 0 {
            return 0.0;
        }
        ((lost as f64 / total as f64) * 100.0).clamp(0.0, 100.0) as f32
    }

    /// Clear all active speakers when leaving a voice channel.
    pub fn clear_speakers(&mut self) {
        self.speakers.clear();
        self.ssrc_to_user.clear();
        self.local_mic_rms_db = MIN_RMS_DB;
        self.local_mic_speaking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_stereo_sine(freq_hz: f32, amplitude: f32, frame_idx: usize) -> [f32; FRAME_SAMPLES] {
        let mut pcm = [0.0f32; FRAME_SAMPLES];
        let base_sample = frame_idx * FRAME_SAMPLES_PER_CHANNEL;
        for i in 0..FRAME_SAMPLES_PER_CHANNEL {
            let t = (base_sample + i) as f32 / (SAMPLE_RATE as f32);
            let s = amplitude * (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            pcm[i * 2] = s;
            pcm[i * 2 + 1] = s;
        }
        pcm
    }

    #[test]
    fn high_pass_filter_removes_dc_and_preserves_440hz() {
        let mut hp = HighPassFilter::new(DEFAULT_HIGH_PASS_CUTOFF_HZ, SAMPLE_RATE);

        // Pure DC offset should decay toward 0.
        let mut dc_frame = [0.5f32; FRAME_SAMPLES];
        for _ in 0..8 {
            dc_frame.fill(0.5);
            hp.process_interleaved_stereo(&mut dc_frame);
        }
        let dc_rms = compute_rms_db(&dc_frame);
        assert!(dc_rms < -25.0, "DC offset should be strongly attenuated, got {dc_rms} dB");

        // 440 Hz tone should pass with near-unity energy.
        let mut hp2 = HighPassFilter::new(DEFAULT_HIGH_PASS_CUTOFF_HZ, SAMPLE_RATE);
        let mut tone = generate_stereo_sine(440.0, 0.5, 0);
        let before = compute_rms_db(&tone);
        hp2.process_interleaved_stereo(&mut tone);
        let after = compute_rms_db(&tone);
        assert!(
            (before - after).abs() < 1.0,
            "440 Hz tone attenuated too much: before={before}, after={after}"
        );
    }

    #[test]
    fn vad_hangover_and_five_silence_frames_plus_ptt_gating() {
        let mut mic = MicCaptureProcessor::new(InputMode::VoiceActivity {
            threshold_db: -30.0,
        })
        .expect("mic init");

        // Loud frame opens gate.
        let loud = generate_stereo_sine(440.0, 0.5, 0);
        let res = mic.process_frame(&loud).expect("loud frame");
        assert!(matches!(res.packet, EncodedMicPacket::Audio(_)));
        assert!(res.speaking);

        // Next 14 quiet frames stay open due to hangover = 15 (quiet_frames = 1..=14).
        let silence = [0.0f32; FRAME_SAMPLES];
        for i in 1..VAD_HANGOVER_FRAMES {
            let r = mic.process_frame(&silence).expect("hangover frame");
            assert!(
                matches!(r.packet, EncodedMicPacket::Audio(_)),
                "expected Audio during hangover frame {i}"
            );
            assert!(r.speaking);
        }

        // Next 5 frames emit explicit 3-byte Opus silence frames.
        for i in 0..TRAILING_SILENCE_FRAMES {
            let r = mic.process_frame(&silence).expect("silence tail");
            assert_eq!(
                r.packet,
                EncodedMicPacket::Silence(OPUS_SILENCE_FRAME.to_vec()),
                "expected Opus silence frame {i}"
            );
            assert!(!r.speaking);
        }

        // Subsequent quiet frames are suppressed.
        let suppressed = mic.process_frame(&silence).expect("suppressed");
        assert_eq!(suppressed.packet, EncodedMicPacket::Suppressed);

        // Switch to PTT mode: loud audio must stay suppressed until ptt_active = true!
        mic.set_input_mode(InputMode::PushToTalk);
        mic.set_ptt_active(false);
        assert_eq!(
            mic.process_frame(&loud).expect("ptt off").packet,
            EncodedMicPacket::Suppressed
        );

        mic.set_ptt_active(true);
        let on = mic.process_frame(&loud).expect("ptt on");
        assert!(matches!(on.packet, EncodedMicPacket::Audio(_)));
        assert!(on.speaking);

        mic.set_ptt_active(false);
        for _ in 0..TRAILING_SILENCE_FRAMES {
            assert_eq!(
                mic.process_frame(&loud).expect("ptt release silence").packet,
                EncodedMicPacket::Silence(OPUS_SILENCE_FRAME.to_vec())
            );
        }
        assert_eq!(
            mic.process_frame(&loud).expect("ptt closed").packet,
            EncodedMicPacket::Suppressed
        );
    }

    #[test]
    fn jitter_buffer_reorders_packets_and_conceals_loss_with_soft_clip_mixer() {
        let mut mic = MicCaptureProcessor::new(InputMode::PushToTalk).expect("mic");
        mic.set_ptt_active(true);

        let mut packets = Vec::new();
        for idx in 0..8 {
            let frame = generate_stereo_sine(440.0, 0.7, idx);
            let EncodedMicPacket::Audio(pkt) = mic.process_frame(&frame).expect("enc").packet
            else {
                panic!("expected audio packet");
            };
            packets.push((1000u16.wrapping_add(idx as u16), pkt));
        }

        let mut mixer = AudioMixer::new(2);
        let alice = Snowflake(101);
        mixer.bind_ssrc(77, alice);
        mixer.set_user_volume(alice, 200);
        mixer.set_master_volume(200);

        // Deliver packets out-of-order and intentionally drop sequence 1002 to exercise PLC!
        mixer
            .push_opus_packet(77, Some(alice), packets[1].0, packets[1].1.clone())
            .expect("push 1");
        mixer
            .push_opus_packet(77, Some(alice), packets[0].0, packets[0].1.clone())
            .expect("push 0");
        // Skip packets[2] (seq 1002) -> PLC!
        for (seq, pkt) in &packets[3..] {
            mixer
                .push_opus_packet(77, Some(alice), *seq, pkt.clone())
                .expect("push");
        }

        let mut mixed_frames = 0usize;
        let mut peak = 0.0f32;
        let mut out = [0.0f32; FRAME_SAMPLES];
        for _ in 0..10 {
            if mixer.mix_frame_flushing(&mut out) > 0 {
                mixed_frames += 1;
                for &s in &out {
                    assert!(
                        (-1.0..=1.0).contains(&s),
                        "tanh soft-clipping must keep sample within [-1, 1], got {s}"
                    );
                    peak = peak.max(s.abs());
                }
            }
        }

        assert!(mixed_frames >= 7, "expected >=7 frames including PLC, got {mixed_frames}");
        assert!(peak > 0.5, "expected audible mixed sine energy, peak={peak}");
        assert!(mixer.packet_loss_pct() > 0.0, "expected recorded packet loss > 0");
        let levels = mixer.audio_levels_snapshot();
        assert!(levels.iter().any(|(uid, rms, _)| *uid == alice && *rms > -30.0));
    }

    #[test]
    fn jitter_buffer_resets_sequence_after_idle_timeout() {
        let mut mic = MicCaptureProcessor::new(InputMode::PushToTalk).expect("mic");
        mic.set_ptt_active(true);

        let mut speaker = SpeakerPipeline::new(55, Some(Snowflake(202)), 2).expect("speaker");
        let mut out = [0.0f32; FRAME_SAMPLES];

        // First talkspurt at high sequence numbers (5000..5004).
        for idx in 0..4 {
            let frame = generate_stereo_sine(440.0, 0.5, idx);
            let EncodedMicPacket::Audio(pkt) = mic.process_frame(&frame).expect("enc").packet
            else {
                panic!("expected audio");
            };
            speaker.push_packet(5000 + idx as u16, pkt);
        }
        for _ in 0..4 {
            assert!(speaker.decode_next_frame(&mut out).expect("decode burst 1"));
        }

        // Advance past SPEAKER_IDLE_RESET_FRAMES silent mixer ticks so sequence state resets.
        for _ in 0..=SPEAKER_IDLE_RESET_FRAMES {
            assert!(!speaker.decode_next_frame(&mut out).expect("idle tick"));
        }
        assert!(speaker.next_play_seq.is_none());

        // Peer reconnects with a much lower random initial RTP sequence (e.g. 42..46) on same SSRC.
        for idx in 0..4 {
            let frame = generate_stereo_sine(440.0, 0.5, idx);
            let EncodedMicPacket::Audio(pkt) = mic.process_frame(&frame).expect("enc").packet
            else {
                panic!("expected audio");
            };
            speaker.push_packet(42 + idx as u16, pkt);
        }
        assert!(
            speaker.decode_next_frame(&mut out).expect("decode burst 2"),
            "packets after idle reset must lock onto new lower sequence instead of dropping"
        );
    }
}
