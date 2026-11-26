//! RTP packetization, UDP IP discovery, and Discord Voice transport AEAD encryption.
//!
//! Implements Discord's current mandatory transport encryption modes:
//! - `aead_xchacha20_poly1305_rtpsize` (preferred)
//! - `aead_aes256_gcm_rtpsize` (fallback)
//!
//! Both modes authenticate the RTP fixed header, CSRC list, and optional 4-byte RTP extension
//! header (profile + length) as AAD while encrypting any extension payload together with the
//! media payload (which itself is DAVE-encrypted when DAVE E2EE is active). A 4-byte incrementing
//! nonce suffix is appended to the tail of each UDP datagram and zero-padded on the right into
//! the cipher's native nonce size (24 bytes for XChaCha20-Poly1305, 12 bytes for AES-256-GCM).

use std::net::{IpAddr, SocketAddr};

use aes_gcm::{Aes256Gcm, KeyInit as _, aead::Aead as _};
use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::Payload};
use zeroize::Zeroizing;

use crate::error::{VoiceError, VoiceResult};

/// Standard RTP protocol version (`2`).
pub const RTP_VERSION: u8 = 2;
/// Discord Opus dynamic RTP payload type (`120` / `0x78`).
pub const RTP_PAYLOAD_TYPE_OPUS: u8 = 120;
/// Minimum RTP fixed header size in bytes.
pub const RTP_HEADER_MIN_LEN: usize = 12;
/// Discord UDP IP discovery datagram size in bytes.
pub const DISCOVERY_PACKET_LEN: usize = 74;
/// Discord UDP IP discovery packet type for client requests.
pub const DISCOVERY_TYPE_REQUEST: u16 = 0x0001;
/// Discord UDP IP discovery packet type for server responses.
pub const DISCOVERY_TYPE_RESPONSE: u16 = 0x0002;
/// Discord UDP IP discovery payload length field (`70` bytes after the 4-byte type/len header).
pub const DISCOVERY_PAYLOAD_LEN: u16 = 70;
/// AEAD authentication tag size in bytes (both Poly1305 and GCM use 16-byte tags).
pub const AEAD_TAG_LEN: usize = 16;
/// Trailing 4-byte incrementing nonce suffix appended to `*_rtpsize` packets.
pub const AEAD_NONCE_SUFFIX_LEN: usize = 4;
/// RTP timestamp increment per 20 ms 48 kHz Opus frame (`48000 / 50 = 960`).
pub const OPUS_TIMESTAMP_STEP: u32 = 960;

/// Supported Discord Voice UDP transport AEAD modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportEncryptionMode {
    /// `aead_xchacha20_poly1305_rtpsize` (32-byte key, 24-byte nonce with 4-byte suffix).
    AeadXChaCha20Poly1305RtpSize,
    /// `aead_aes256_gcm_rtpsize` (32-byte key, 12-byte nonce with 4-byte suffix).
    AeadAes256GcmRtpSize,
}

impl TransportEncryptionMode {
    /// Wire name advertised by Discord Voice Gateway `READY` and `SELECT_PROTOCOL`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AeadXChaCha20Poly1305RtpSize => "aead_xchacha20_poly1305_rtpsize",
            Self::AeadAes256GcmRtpSize => "aead_aes256_gcm_rtpsize",
        }
    }

    /// Parse a Discord Voice Gateway encryption mode string.
    #[must_use]
    pub fn from_discord_str(mode: &str) -> Option<Self> {
        match mode {
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::AeadXChaCha20Poly1305RtpSize),
            "aead_aes256_gcm_rtpsize" => Some(Self::AeadAes256GcmRtpSize),
            _ => None,
        }
    }

    /// Pick the strongest mutually supported AEAD mode offered in `READY.modes`.
    #[must_use]
    pub fn negotiate(server_modes: &[String]) -> Option<Self> {
        if server_modes
            .iter()
            .any(|m| m == Self::AeadXChaCha20Poly1305RtpSize.as_str())
        {
            return Some(Self::AeadXChaCha20Poly1305RtpSize);
        }
        if server_modes
            .iter()
            .any(|m| m == Self::AeadAes256GcmRtpSize.as_str())
        {
            return Some(Self::AeadAes256GcmRtpSize);
        }
        None
    }
}

/// Build a 74-byte Discord UDP IP Discovery request packet for `ssrc`.
#[must_use]
pub fn build_ip_discovery_request(ssrc: u32) -> [u8; DISCOVERY_PACKET_LEN] {
    let mut buf = [0u8; DISCOVERY_PACKET_LEN];
    buf[0..2].copy_from_slice(&DISCOVERY_TYPE_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&DISCOVERY_PAYLOAD_LEN.to_be_bytes());
    buf[4..8].copy_from_slice(&ssrc.to_be_bytes());
    buf
}

/// Parse a 74-byte Discord UDP IP Discovery response packet into the discovered external address.
pub fn parse_ip_discovery_response(
    packet: &[u8],
    expected_ssrc: Option<u32>,
) -> VoiceResult<SocketAddr> {
    if packet.len() != DISCOVERY_PACKET_LEN {
        return Err(VoiceError::Discovery(format!(
            "unexpected discovery packet length {}, expected {DISCOVERY_PACKET_LEN}",
            packet.len()
        )));
    }

    let pkt_type = u16::from_be_bytes([packet[0], packet[1]]);
    if pkt_type != DISCOVERY_TYPE_RESPONSE {
        return Err(VoiceError::Discovery(format!(
            "unexpected discovery packet type 0x{pkt_type:04x}, expected response 0x{DISCOVERY_TYPE_RESPONSE:04x}"
        )));
    }

    let pkt_len = u16::from_be_bytes([packet[2], packet[3]]);
    if pkt_len != DISCOVERY_PAYLOAD_LEN {
        return Err(VoiceError::Discovery(format!(
            "unexpected discovery payload length {pkt_len}, expected {DISCOVERY_PAYLOAD_LEN}"
        )));
    }

    let ssrc = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    if let Some(expected) = expected_ssrc
        && ssrc != expected
    {
        return Err(VoiceError::Discovery(format!(
            "discovery response SSRC mismatch: got {ssrc}, expected {expected}"
        )));
    }

    let ip_slice = &packet[8..72];
    let nul_pos = ip_slice
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(ip_slice.len());
    let ip_str = std::str::from_utf8(&ip_slice[..nul_pos])
        .map_err(|_| VoiceError::Discovery("discovery IP field is not valid UTF-8".to_string()))?;
    let ip: IpAddr = ip_str
        .parse()
        .map_err(|_| VoiceError::Discovery(format!("invalid discovered IP address: {ip_str:?}")))?;
    let port = u16::from_be_bytes([packet[72], packet[73]]);

    Ok(SocketAddr::new(ip, port))
}

/// Returns `true` if `packet` is an RTCP packet multiplexed on the Discord voice UDP port
/// (RFC 5761 payload types 200..=206: SR, RR, SDES, BYE, APP, RTPFB, PSFB).
#[must_use]
pub fn is_rtcp_packet(packet: &[u8]) -> bool {
    packet.len() >= 2 && (200..=206).contains(&packet[1])
}

/// Parsed RFC 3550 RTP header metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpHeader {
    /// RTP version (must be `2`).
    pub version: u8,
    /// Padding bit (`P`).
    pub padding: bool,
    /// Header extension bit (`X`).
    pub extension: bool,
    /// CSRC count (`CC`, `0..=15`).
    pub csrc_count: u8,
    /// Marker bit (`M`).
    pub marker: bool,
    /// 7-bit payload type (`PT`, `120` for Discord Opus).
    pub payload_type: u8,
    /// 16-bit big-endian sequence number.
    pub sequence: u16,
    /// 32-bit big-endian RTP timestamp.
    pub timestamp: u32,
    /// 32-bit big-endian synchronization source identifier.
    pub ssrc: u32,
    /// Contributing source identifiers (length == `csrc_count`).
    pub csrcs: Vec<u32>,
    /// Optional 16-bit header extension profile identifier (when `extension == true`).
    pub extension_profile: Option<u16>,
    /// Number of 32-bit extension data words following the 4-byte extension header.
    pub extension_word_len: usize,
}

impl RtpHeader {
    /// Build a standard Discord Opus RTP header (`V=2, P=0, X=0, CC=0, M=0, PT=120`).
    #[must_use]
    pub fn new_opus(sequence: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            version: RTP_VERSION,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type: RTP_PAYLOAD_TYPE_OPUS,
            sequence,
            timestamp,
            ssrc,
            csrcs: Vec::new(),
            extension_profile: None,
            extension_word_len: 0,
        }
    }

    /// Serialize the unencrypted RTP prefix (which is also the AAD for `*_rtpsize` AEAD modes).
    pub fn write_aad_prefix(&self, out: &mut Vec<u8>) -> VoiceResult<()> {
        if self.csrcs.len() > 15 {
            return Err(VoiceError::Rtp("RTP CSRC count exceeds 15".to_string()));
        }
        let cc = self.csrcs.len() as u8;
        let first_byte = ((self.version & 0x03) << 6)
            | (u8::from(self.padding) << 5)
            | (u8::from(self.extension) << 4)
            | (cc & 0x0F);
        let second_byte = (u8::from(self.marker) << 7) | (self.payload_type & 0x7F);

        out.push(first_byte);
        out.push(second_byte);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        for csrc in &self.csrcs {
            out.extend_from_slice(&csrc.to_be_bytes());
        }
        if self.extension {
            let profile = self.extension_profile.unwrap_or(0xBEDE);
            let words = u16::try_from(self.extension_word_len).map_err(|_| {
                VoiceError::Rtp("RTP extension word length exceeds u16::MAX".to_string())
            })?;
            out.extend_from_slice(&profile.to_be_bytes());
            out.extend_from_slice(&words.to_be_bytes());
        }
        Ok(())
    }

    /// Parse the unencrypted RTP prefix from `packet` and return `(header, aad_len)`.
    ///
    /// Per Discord's `*_rtpsize` specification, `aad_len` includes the 12-byte fixed header,
    /// `4 * CC` CSRC bytes, and (when `X == 1`) the 4-byte extension header, while leaving the
    /// `4 * extension_word_len` extension payload inside the encrypted portion.
    pub fn parse_aad_prefix(packet: &[u8]) -> VoiceResult<(Self, usize)> {
        if packet.len() < RTP_HEADER_MIN_LEN {
            return Err(VoiceError::Rtp(format!(
                "RTP packet too short ({} bytes, minimum {RTP_HEADER_MIN_LEN})",
                packet.len()
            )));
        }

        let b0 = packet[0];
        let b1 = packet[1];
        let version = b0 >> 6;
        if version != RTP_VERSION {
            return Err(VoiceError::Rtp(format!(
                "unsupported RTP version {version}, expected {RTP_VERSION}"
            )));
        }

        let padding = (b0 & 0x20) != 0;
        let extension = (b0 & 0x10) != 0;
        let csrc_count = b0 & 0x0F;
        let marker = (b1 & 0x80) != 0;
        let payload_type = b1 & 0x7F;
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

        let csrc_end = RTP_HEADER_MIN_LEN + usize::from(csrc_count) * 4;
        if packet.len() < csrc_end {
            return Err(VoiceError::Rtp(format!(
                "RTP packet truncated in CSRC list ({} < {csrc_end})",
                packet.len()
            )));
        }

        let mut csrcs = Vec::with_capacity(usize::from(csrc_count));
        for &chunk in packet[RTP_HEADER_MIN_LEN..csrc_end].as_chunks::<4>().0 {
            csrcs.push(u32::from_be_bytes(chunk));
        }

        let (aad_len, extension_profile, extension_word_len) = if extension {
            let ext_hdr_end = csrc_end + 4;
            if packet.len() < ext_hdr_end {
                return Err(VoiceError::Rtp(format!(
                    "RTP packet truncated in extension header ({} < {ext_hdr_end})",
                    packet.len()
                )));
            }
            let profile = u16::from_be_bytes([packet[csrc_end], packet[csrc_end + 1]]);
            let words = u16::from_be_bytes([packet[csrc_end + 2], packet[csrc_end + 3]]);
            (ext_hdr_end, Some(profile), usize::from(words))
        } else {
            (csrc_end, None, 0)
        };

        Ok((
            Self {
                version,
                padding,
                extension,
                csrc_count,
                marker,
                payload_type,
                sequence,
                timestamp,
                ssrc,
                csrcs,
                extension_profile,
                extension_word_len,
            },
            aad_len,
        ))
    }
}

/// Decrypted RTP packet returned by [`RtpTransportCipher::decrypt_packet`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedRtpPacket {
    /// Parsed RTP header metadata.
    pub header: RtpHeader,
    /// Decrypted media payload with any RTP header extension payload and RTP padding stripped.
    pub payload: Vec<u8>,
}

/// Stateful RTP sequence/timestamp generator for outbound Opus audio frames.
#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
}

impl RtpPacketizer {
    /// Create a packetizer for `ssrc` starting at a random initial sequence and timestamp.
    #[must_use]
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            sequence: rand::random(),
            timestamp: rand::random(),
        }
    }

    /// Create a packetizer with deterministic starting counters (useful for tests).
    #[must_use]
    pub const fn with_initial(ssrc: u32, sequence: u16, timestamp: u32) -> Self {
        Self {
            ssrc,
            sequence,
            timestamp,
        }
    }

    /// Return the local SSRC.
    #[must_use]
    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Produce the next RTP header and advance sequence (`+1`) and timestamp (`+960`).
    pub fn next_opus_header(&mut self) -> RtpHeader {
        let header = RtpHeader::new_opus(self.sequence, self.timestamp, self.ssrc);
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(OPUS_TIMESTAMP_STEP);
        header
    }
}

enum CipherBackend {
    XChaCha20Poly1305(Box<XChaCha20Poly1305>),
    Aes256Gcm(Box<Aes256Gcm>),
}

/// Discord UDP Voice Transport AEAD encryptor/decryptor (`*_rtpsize`).
///
/// The 32-byte transport secret key is wrapped in [`Zeroizing`] and wiped on drop.
pub struct RtpTransportCipher {
    mode: TransportEncryptionMode,
    _secret_key: Zeroizing<[u8; 32]>,
    cipher: CipherBackend,
    tx_nonce: u32,
}

impl std::fmt::Debug for RtpTransportCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpTransportCipher")
            .field("mode", &self.mode)
            .field("secret_key", &"[REDACTED]")
            .field("tx_nonce", &self.tx_nonce)
            .finish()
    }
}

impl RtpTransportCipher {
    /// Construct a transport AEAD cipher from the negotiated `mode` and 32-byte `secret_key`
    /// delivered in Voice Gateway `SESSION_DESCRIPTION` (Op 4).
    pub fn new(mode: TransportEncryptionMode, secret_key: &[u8]) -> VoiceResult<Self> {
        let key_bytes: [u8; 32] = secret_key.try_into().map_err(|_| {
            VoiceError::TransportCrypto(format!(
                "invalid transport secret key length {}, expected 32",
                secret_key.len()
            ))
        })?;

        let cipher = match mode {
            TransportEncryptionMode::AeadXChaCha20Poly1305RtpSize => {
                CipherBackend::XChaCha20Poly1305(Box::new(XChaCha20Poly1305::new(
                    (&key_bytes).into(),
                )))
            }
            TransportEncryptionMode::AeadAes256GcmRtpSize => {
                CipherBackend::Aes256Gcm(Box::new(Aes256Gcm::new((&key_bytes).into())))
            }
        };

        Ok(Self {
            mode,
            _secret_key: Zeroizing::new(key_bytes),
            cipher,
            tx_nonce: 0,
        })
    }

    /// Return the active transport encryption mode.
    #[must_use]
    pub const fn mode(&self) -> TransportEncryptionMode {
        self.mode
    }

    /// Encrypt `payload` under `header` into a Discord `*_rtpsize` UDP datagram.
    pub fn encrypt_packet(&mut self, header: &RtpHeader, payload: &[u8]) -> VoiceResult<Vec<u8>> {
        let nonce_val = self.tx_nonce;
        self.tx_nonce = self.tx_nonce.wrapping_add(1);
        self.encrypt_packet_with_nonce(header, payload, nonce_val)
    }

    /// Encrypt `payload` with an explicit 32-bit `nonce_counter` (also useful for deterministic tests).
    pub fn encrypt_packet_with_nonce(
        &self,
        header: &RtpHeader,
        payload: &[u8],
        nonce_counter: u32,
    ) -> VoiceResult<Vec<u8>> {
        let mut out =
            Vec::with_capacity(RTP_HEADER_MIN_LEN + payload.len() + AEAD_TAG_LEN + AEAD_NONCE_SUFFIX_LEN);
        header.write_aad_prefix(&mut out)?;
        let aad_len = out.len();
        let nonce_suffix = nonce_counter.to_be_bytes();

        let ciphertext = match &self.cipher {
            CipherBackend::XChaCha20Poly1305(c) => {
                let mut nonce = [0u8; 24];
                nonce[0..4].copy_from_slice(&nonce_suffix);
                c.encrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: payload,
                        aad: &out[..aad_len],
                    },
                )
                .map_err(|_| {
                    VoiceError::TransportCrypto("XChaCha20-Poly1305 encryption failed".to_string())
                })?
            }
            CipherBackend::Aes256Gcm(c) => {
                let mut nonce = [0u8; 12];
                nonce[0..4].copy_from_slice(&nonce_suffix);
                c.encrypt(
                    (&nonce).into(),
                    Payload {
                        msg: payload,
                        aad: &out[..aad_len],
                    },
                )
                .map_err(|_| {
                    VoiceError::TransportCrypto("AES-256-GCM encryption failed".to_string())
                })?
            }
        };

        out.extend_from_slice(&ciphertext);
        out.extend_from_slice(&nonce_suffix);
        Ok(out)
    }

    /// Authenticate and decrypt an incoming Discord `*_rtpsize` UDP datagram.
    pub fn decrypt_packet(&self, packet: &[u8]) -> VoiceResult<DecryptedRtpPacket> {
        if is_rtcp_packet(packet) {
            return Err(VoiceError::Rtp("packet is RTCP, not RTP media".to_string()));
        }

        let (header, aad_len) = RtpHeader::parse_aad_prefix(packet)?;
        let min_total = aad_len + AEAD_TAG_LEN + AEAD_NONCE_SUFFIX_LEN;
        if packet.len() < min_total {
            return Err(VoiceError::TransportCrypto(format!(
                "encrypted RTP packet too short ({} < {min_total})",
                packet.len()
            )));
        }

        let aad = &packet[..aad_len];
        let nonce_start = packet.len() - AEAD_NONCE_SUFFIX_LEN;
        let ciphertext = &packet[aad_len..nonce_start];
        let nonce_suffix = &packet[nonce_start..];

        let mut plaintext = match &self.cipher {
            CipherBackend::XChaCha20Poly1305(c) => {
                let mut nonce = [0u8; 24];
                nonce[0..4].copy_from_slice(nonce_suffix);
                c.decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: ciphertext,
                        aad,
                    },
                )
                .map_err(|_| {
                    VoiceError::TransportCrypto(
                        "XChaCha20-Poly1305 authentication/decryption failed".to_string(),
                    )
                })?
            }
            CipherBackend::Aes256Gcm(c) => {
                let mut nonce = [0u8; 12];
                nonce[0..4].copy_from_slice(nonce_suffix);
                c.decrypt(
                    (&nonce).into(),
                    Payload {
                        msg: ciphertext,
                        aad,
                    },
                )
                .map_err(|_| {
                    VoiceError::TransportCrypto(
                        "AES-256-GCM authentication/decryption failed".to_string(),
                    )
                })?
            }
        };

        // In `*_rtpsize` modes, any RTP header extension data bytes precede the media payload
        // inside the decrypted plaintext and must be stripped safely.
        if header.extension {
            let ext_bytes = header.extension_word_len.saturating_mul(4);
            if plaintext.len() < ext_bytes {
                return Err(VoiceError::Rtp(format!(
                    "decrypted RTP payload shorter than declared extension data ({} < {ext_bytes})",
                    plaintext.len()
                )));
            }
            plaintext.drain(..ext_bytes);
        }

        // Strip RFC 3550 padding if the padding bit (`P`) is set.
        if header.padding {
            let Some(&pad_len) = plaintext.last() else {
                return Err(VoiceError::Rtp(
                    "RTP padding bit set on empty decrypted payload".to_string(),
                ));
            };
            let pad = usize::from(pad_len);
            if pad == 0 || pad > plaintext.len() {
                return Err(VoiceError::Rtp(format!(
                    "invalid RTP padding length {pad} for payload size {}",
                    plaintext.len()
                )));
            }
            let new_len = plaintext.len() - pad;
            plaintext.truncate(new_len);
        }

        Ok(DecryptedRtpPacket {
            header,
            payload: plaintext,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn ip_discovery_roundtrip_ipv4_and_ipv6() {
        let req = build_ip_discovery_request(0x1122_3344);
        assert_eq!(req.len(), DISCOVERY_PACKET_LEN);
        assert_eq!(&req[0..2], &[0x00, 0x01]);
        assert_eq!(&req[2..4], &[0x00, 0x46]);
        assert_eq!(&req[4..8], &[0x11, 0x22, 0x33, 0x44]);

        let mut resp = [0u8; DISCOVERY_PACKET_LEN];
        resp[0..2].copy_from_slice(&DISCOVERY_TYPE_RESPONSE.to_be_bytes());
        resp[2..4].copy_from_slice(&DISCOVERY_PAYLOAD_LEN.to_be_bytes());
        resp[4..8].copy_from_slice(&0x1122_3344u32.to_be_bytes());
        let ip = b"203.0.113.42";
        resp[8..8 + ip.len()].copy_from_slice(ip);
        resp[72..74].copy_from_slice(&50001u16.to_be_bytes());

        let addr = parse_ip_discovery_response(&resp, Some(0x1122_3344)).expect("valid IPv4");
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42)), 50001));

        // IPv6 response
        let mut resp6 = [0u8; DISCOVERY_PACKET_LEN];
        resp6[0..2].copy_from_slice(&DISCOVERY_TYPE_RESPONSE.to_be_bytes());
        resp6[2..4].copy_from_slice(&DISCOVERY_PAYLOAD_LEN.to_be_bytes());
        resp6[4..8].copy_from_slice(&7u32.to_be_bytes());
        let ip6 = b"2001:db8::1";
        resp6[8..8 + ip6.len()].copy_from_slice(ip6);
        resp6[72..74].copy_from_slice(&54321u16.to_be_bytes());
        let addr6 = parse_ip_discovery_response(&resp6, Some(7)).expect("valid IPv6");
        assert_eq!(
            addr6,
            SocketAddr::new(IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().expect("v6")), 54321)
        );

        // Truncated packet must error cleanly without panic
        assert!(parse_ip_discovery_response(&resp[..10], None).is_err());
        assert!(parse_ip_discovery_response(&resp, Some(999)).is_err());
    }

    #[test]
    fn aead_xchacha_and_aes_gcm_roundtrip_and_tamper_rejection() {
        let key = [0x42u8; 32];
        let payload = b"opus-frame-or-dave-ciphertext";

        for mode in [
            TransportEncryptionMode::AeadXChaCha20Poly1305RtpSize,
            TransportEncryptionMode::AeadAes256GcmRtpSize,
        ] {
            let mut tx = RtpTransportCipher::new(mode, &key).expect("tx cipher");
            let rx = RtpTransportCipher::new(mode, &key).expect("rx cipher");
            let mut packetizer = RtpPacketizer::with_initial(42, 100, 9600);
            let header = packetizer.next_opus_header();

            let mut wire = tx.encrypt_packet(&header, payload).expect("encrypt");
            let decoded = rx.decrypt_packet(&wire).expect("decrypt");
            assert_eq!(decoded.header, header);
            assert_eq!(decoded.payload, payload);

            // Tampering with RTP header (AAD) must fail authentication.
            wire[3] ^= 0x01;
            assert!(rx.decrypt_packet(&wire).is_err());
            wire[3] ^= 0x01;

            // Tampering with ciphertext must fail authentication.
            wire[RTP_HEADER_MIN_LEN] ^= 0x80;
            assert!(rx.decrypt_packet(&wire).is_err());
            wire[RTP_HEADER_MIN_LEN] ^= 0x80;

            // Tampering with trailing nonce must fail authentication.
            let last = wire.len() - 1;
            wire[last] ^= 0x01;
            assert!(rx.decrypt_packet(&wire).is_err());
        }
    }

    #[test]
    fn rtp_extension_data_and_padding_stripped_safely() {
        let key = [0xABu8; 32];
        let mut tx = RtpTransportCipher::new(
            TransportEncryptionMode::AeadXChaCha20Poly1305RtpSize,
            &key,
        )
        .expect("cipher");
        let rx = RtpTransportCipher::new(
            TransportEncryptionMode::AeadXChaCha20Poly1305RtpSize,
            &key,
        )
        .expect("cipher");

        let mut header = RtpHeader::new_opus(1, 960, 12345);
        header.extension = true;
        header.extension_profile = Some(0xBEDE);
        header.extension_word_len = 1; // 4 bytes of extension data inside plaintext
        header.padding = true;

        // 4 bytes extension data + media + 2 bytes RFC 3550 padding
        let mut raw_plaintext = vec![0x10, 0xAA, 0x00, 0x00];
        raw_plaintext.extend_from_slice(b"hello-opus");
        raw_plaintext.extend_from_slice(&[0x00, 0x02]);

        let wire = tx.encrypt_packet(&header, &raw_plaintext).expect("encrypt");
        let dec = rx.decrypt_packet(&wire).expect("decrypt");
        assert_eq!(dec.payload, b"hello-opus");
    }

    #[test]
    fn mode_negotiation_prefers_xchacha_over_aes() {
        let both = vec![
            "aead_aes256_gcm_rtpsize".to_string(),
            "aead_xchacha20_poly1305_rtpsize".to_string(),
        ];
        assert_eq!(
            TransportEncryptionMode::negotiate(&both),
            Some(TransportEncryptionMode::AeadXChaCha20Poly1305RtpSize)
        );
        let aes_only = vec!["aead_aes256_gcm_rtpsize".to_string()];
        assert_eq!(
            TransportEncryptionMode::negotiate(&aes_only),
            Some(TransportEncryptionMode::AeadAes256GcmRtpSize)
        );
        let legacy_only = vec!["xsalsa20_poly1305".to_string()];
        assert_eq!(TransportEncryptionMode::negotiate(&legacy_only), None);
    }
}
