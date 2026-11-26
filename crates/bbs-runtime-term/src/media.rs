//! Multi-tier terminal media renderer, bounded background image/GIF decoder, byte-bounded
//! decoded-frame LRU cache, aspect-ratio clamping, and PTY bandwidth governor.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Cursor;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bbs_core::model::RgbaFrame;
use bbs_core::viewport::{GraphicsTier, ResolvedCharset};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};
use lru::LruCache;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Position, Rect, Size};
use ratatui::widgets::Widget;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::halfblocks::Halfblocks;
use ratatui_image::protocol::iterm2::Iterm2;
use ratatui_image::protocol::kitty::{Kitty, StatefulKitty};
use ratatui_image::protocol::sixel::Sixel;
use ratatui_image::protocol::{Protocol, StatefulProtocol, StatefulProtocolType};
use ratatui_image::{FontSize, Image as RatatuiImageWidget};
use tokio::sync::{Semaphore, mpsc};
use unicode_width::UnicodeWidthChar;

use crate::probe::{DEFAULT_CELL_PIXELS, ProbeReport};
use crate::{TermError, TermResult};

/// Default decoded RGBA frame cache budget (`64 MiB`).
pub const DEFAULT_MEDIA_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum width or height retained for static images after decode (`1024 px`).
pub const DEFAULT_MAX_STATIC_DIM: u32 = 1024;

/// Default maximum width or height retained for animated GIF/WebP preview frames.
pub const DEFAULT_MAX_ANIMATED_DIM: u32 = 320;

/// Default maximum number of decoded frames kept per animated GIF/WebP.
pub const DEFAULT_MAX_ANIMATED_FRAMES: usize = 60;

/// Default per-item decoded RGBA memory ceiling (`16 MiB`).
pub const DEFAULT_MAX_DECODED_BYTES_PER_ITEM: usize = 16 * 1024 * 1024;

/// Default hard image-header dimension guard against decompression bombs.
pub const DEFAULT_MAX_HEADER_DIM: u32 = 16_384;

/// Default maximum concurrent background image/GIF decode tasks (`8` parallel workers).
pub const DEFAULT_DECODE_CONCURRENCY: usize = 8;

/// Default maximum concurrent background terminal-graphics encode tasks (`4` workers).
pub const DEFAULT_ENCODE_CONCURRENCY: usize = 4;

/// Standard GIF fallback delay when a GIF frame declares `0 ms` (`100 ms`).
pub const DEFAULT_GIF_DELAY_MS: u16 = 100;

/// Minimum clamped per-frame GIF delay (`20 ms`).
pub const MIN_GIF_DELAY_MS: u16 = 20;

/// Maximum clamped per-frame GIF delay (`10_000 ms`).
pub const MAX_GIF_DELAY_MS: u16 = 10_000;

/// Default local PTY media byte-rate ceiling (`2 MiB/s`).
pub const LOCAL_PTY_BYTE_BUDGET_PER_SEC: usize = 2 * 1024 * 1024;

/// Default SSH PTY media byte-rate ceiling (`128 KiB/s`).
pub const SSH_PTY_BYTE_BUDGET_PER_SEC: usize = 128 * 1024;

/// Base64 payload chunk size mandated by the Kitty graphics protocol.
const KITTY_CHUNK_BYTES: usize = 4096;

/// How an image fits inside its target cell bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaFitMode {
    /// Fit inside target bounds while preserving aspect ratio; never upscales tiny images.
    #[default]
    Contain,
    /// Scale up or down to fill the limiting axis while preserving aspect ratio.
    ScaleToFit,
}

/// Compute a safe, non-zero cell bounding box preserving source aspect ratio for the active cell
/// pixel dimensions.
///
/// Fixes `discordo`'s zero-height aspect-ratio bug (`bounds.Dy() == 0`) by clamping both axes to
/// `>= 1` whenever `max_cols > 0 && max_rows > 0`, even on extreme `500×1` panoramic or `1×500`
/// vertical images.
#[must_use]
pub fn compute_image_cell_box(
    img_width: u32,
    img_height: u32,
    max_cols: u16,
    max_rows: u16,
    cell_px: (u16, u16),
    fit: MediaFitMode,
) -> Size {
    if max_cols == 0 || max_rows == 0 {
        return Size::new(0, 0);
    }
    if img_width == 0 || img_height == 0 {
        return Size::new(1, 1);
    }

    let cw = f64::from(cell_px.0.max(1));
    let ch = f64::from(cell_px.1.max(1));
    let avail_px_w = f64::from(max_cols) * cw;
    let avail_px_h = f64::from(max_rows) * ch;

    let raw_scale = (avail_px_w / f64::from(img_width)).min(avail_px_h / f64::from(img_height));
    let scale = match fit {
        MediaFitMode::Contain => raw_scale.min(1.0),
        MediaFitMode::ScaleToFit => raw_scale,
    };

    let target_cols = ((f64::from(img_width) * scale) / cw)
        .round()
        .clamp(1.0, f64::from(max_cols)) as u16;
    let target_rows = ((f64::from(img_height) * scale) / ch)
        .round()
        .clamp(1.0, f64::from(max_rows)) as u16;

    Size::new(target_cols.max(1), target_rows.max(1))
}

/// Compute safe, non-zero pixel dimensions fitting `(img_width, img_height)` inside
/// `(max_px_w, max_px_h)` while preserving aspect ratio and guaranteeing `>= (1, 1)`.
#[must_use]
pub fn aspect_clamped_pixels(
    img_width: u32,
    img_height: u32,
    max_px_w: u32,
    max_px_h: u32,
) -> (u32, u32) {
    let cap_w = max_px_w.max(1);
    let cap_h = max_px_h.max(1);
    if img_width == 0 || img_height == 0 {
        return (1, 1);
    }
    let scale = (f64::from(cap_w) / f64::from(img_width))
        .min(f64::from(cap_h) / f64::from(img_height))
        .min(1.0);
    let out_w = (f64::from(img_width) * scale)
        .round()
        .clamp(1.0, f64::from(cap_w)) as u32;
    let out_h = (f64::from(img_height) * scale)
        .round()
        .clamp(1.0, f64::from(cap_h)) as u32;
    (out_w.max(1), out_h.max(1))
}

/// Memory and frame-count ceilings enforced during image and GIF decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum width or height retained for static images (PNG/JPEG/WebP).
    pub max_static_dimension: u32,
    /// Maximum width or height retained for animated GIF frames.
    pub max_animated_dimension: u32,
    /// Maximum frame count decoded per animated GIF.
    pub max_animated_frames: usize,
    /// Maximum aggregate RGBA byte footprint per decoded media item.
    pub max_decoded_bytes_per_item: usize,
    /// Hard header-dimension rejection guard against decompression bombs.
    pub max_header_dimension: u32,
    /// Minimum clamped per-frame delay in milliseconds.
    pub min_frame_delay_ms: u16,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_static_dimension: DEFAULT_MAX_STATIC_DIM,
            max_animated_dimension: DEFAULT_MAX_ANIMATED_DIM,
            max_animated_frames: DEFAULT_MAX_ANIMATED_FRAMES,
            max_decoded_bytes_per_item: DEFAULT_MAX_DECODED_BYTES_PER_ITEM,
            max_header_dimension: DEFAULT_MAX_HEADER_DIM,
            min_frame_delay_ms: MIN_GIF_DELAY_MS,
        }
    }
}

/// Decoded RGBA8 static image or multi-frame animation sequence held in RAM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMedia {
    /// Cache key (typically canonical URL or media identifier).
    pub key: String,
    /// Optional human-readable title/filename used for `TextTagOnly` fallback rendering.
    pub title: Option<String>,
    /// Original intrinsic source pixel width before decode downscaling.
    pub source_width: u32,
    /// Original intrinsic source pixel height before decode downscaling.
    pub source_height: u32,
    /// `true` when `frames.len() > 1`.
    pub is_animated: bool,
    /// Ordered RGBA8 frames (`>= 1` for valid decoded media).
    pub frames: Vec<RgbaFrame>,
    /// Total tracked memory footprint in bytes for LRU cache accounting.
    pub byte_size: usize,
}

impl DecodedMedia {
    /// Construct a [`DecodedMedia`] from pre-decoded [`RgbaFrame`]s (e.g. from `MockGifProvider`
    /// or a decoded attachment).
    #[must_use]
    pub fn from_rgba_frames(
        key: impl Into<String>,
        title: Option<String>,
        source_width: u32,
        source_height: u32,
        frames: Vec<RgbaFrame>,
    ) -> Self {
        let key = key.into();
        let is_animated = frames.len() > 1;
        let pixel_bytes: usize = frames.iter().map(|f| f.pixels.len()).sum();
        let byte_size = pixel_bytes.max(1);
        Self {
            key,
            title,
            source_width,
            source_height,
            is_animated,
            frames,
            byte_size,
        }
    }

    /// Number of decoded frames.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Retrieve frame at `index` (wrapping cleanly if out of range, or `None` if empty).
    #[must_use]
    pub fn frame(&self, index: usize) -> Option<&RgbaFrame> {
        if self.frames.is_empty() {
            None
        } else {
            self.frames.get(index % self.frames.len())
        }
    }
}

/// Convert a [`bbs_core::model::RgbaFrame`] into an [`image::DynamicImage`] without panicking.
pub fn rgba_frame_to_dynamic_image(frame: &RgbaFrame) -> TermResult<DynamicImage> {
    if frame.width == 0 || frame.height == 0 {
        return Err(TermError::MediaDecode(
            "RGBA frame has zero width or height".to_owned(),
        ));
    }
    let expected_len = usize::try_from(frame.width)
        .ok()
        .and_then(|w| usize::try_from(frame.height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| TermError::MediaDecode("RGBA frame dimensions overflow".to_owned()))?;

    if frame.pixels.len() != expected_len {
        return Err(TermError::MediaDecode(format!(
            "RGBA pixel length mismatch: expected {expected_len}, got {}",
            frame.pixels.len()
        )));
    }

    let buf = RgbaImage::from_raw(frame.width, frame.height, frame.pixels.clone()).ok_or_else(
        || TermError::MediaDecode("failed to construct RgbaImage from frame pixels".to_owned()),
    )?;
    Ok(DynamicImage::ImageRgba8(buf))
}

/// Decode an encoded image or multi-frame GIF/WebP byte buffer (GIF, PNG, JPEG, WebP) into a
/// bounded [`DecodedMedia`] RGBA frame sequence.
pub fn decode_image_bytes(
    key: impl Into<String>,
    bytes: &[u8],
    title: Option<String>,
    limits: &DecodeLimits,
) -> TermResult<DecodedMedia> {
    let key = key.into();
    if bytes.is_empty() {
        return Err(TermError::MediaDecode("empty image payload".to_owned()));
    }

    let format = image::guess_format(bytes)
        .map_err(|err| TermError::MediaDecode(format!("unrecognized image format: {err}")))?;

    match format {
        ImageFormat::Gif => decode_gif_bytes(key, bytes, title, limits),
        ImageFormat::WebP => decode_webp_bytes(key, bytes, title, limits),
        ImageFormat::Png | ImageFormat::Jpeg => {
            decode_static_bytes(key, bytes, format, title, limits)
        }
        other => Err(TermError::MediaDecode(format!(
            "unsupported image format: {other:?}"
        ))),
    }
}

fn decode_gif_bytes(
    key: String,
    bytes: &[u8],
    title: Option<String>,
    limits: &DecodeLimits,
) -> TermResult<DecodedMedia> {
    let decoder = GifDecoder::new(Cursor::new(bytes))
        .map_err(|err| TermError::MediaDecode(format!("invalid GIF header: {err}")))?;

    let (source_width, source_height) = decoder.dimensions();
    decode_animated_frames(
        key,
        source_width,
        source_height,
        decoder.into_frames(),
        title,
        limits,
        "GIF",
    )
}

fn decode_webp_bytes(
    key: String,
    bytes: &[u8],
    title: Option<String>,
    limits: &DecodeLimits,
) -> TermResult<DecodedMedia> {
    let decoder = WebPDecoder::new(Cursor::new(bytes))
        .map_err(|err| TermError::MediaDecode(format!("invalid WebP header: {err}")))?;

    if !decoder.has_animation() {
        return decode_static_bytes(key, bytes, ImageFormat::WebP, title, limits);
    }

    let (source_width, source_height) = decoder.dimensions();
    decode_animated_frames(
        key,
        source_width,
        source_height,
        decoder.into_frames(),
        title,
        limits,
        "WebP",
    )
}

fn decode_animated_frames(
    key: String,
    source_width: u32,
    source_height: u32,
    frame_iter: image::Frames<'_>,
    title: Option<String>,
    limits: &DecodeLimits,
    format_label: &str,
) -> TermResult<DecodedMedia> {
    if source_width == 0 || source_height == 0 {
        return Err(TermError::MediaDecode(format!(
            "{format_label} has zero width or height"
        )));
    }
    if source_width > limits.max_header_dimension || source_height > limits.max_header_dimension {
        return Err(TermError::MediaDecode(format!(
            "{format_label} dimensions {source_width}x{source_height} exceed safety limit {}",
            limits.max_header_dimension
        )));
    }

    let (target_w, target_h) = aspect_clamped_pixels(
        source_width,
        source_height,
        limits.max_animated_dimension,
        limits.max_animated_dimension,
    );
    let needs_resize = target_w != source_width || target_h != source_height;

    let mut frames = Vec::new();
    let mut accumulated_bytes = 0usize;
    let max_frames = limits.max_animated_frames.max(1);

    for frame_res in frame_iter {
        if frames.len() >= max_frames {
            break;
        }
        let frame = frame_res
            .map_err(|err| TermError::MediaDecode(format!("corrupt {format_label} frame: {err}")))?;

        let (numer, denom) = frame.delay().numer_denom_ms();
        let raw_delay_ms = numer
            .checked_div(denom)
            .unwrap_or(u32::from(DEFAULT_GIF_DELAY_MS));
        let normalized_delay = if raw_delay_ms == 0 {
            DEFAULT_GIF_DELAY_MS
        } else {
            raw_delay_ms.clamp(u32::from(limits.min_frame_delay_ms), u32::from(MAX_GIF_DELAY_MS))
                as u16
        };

        let rgba = frame.into_buffer();
        let final_rgba = if needs_resize {
            DynamicImage::ImageRgba8(rgba)
                .resize_exact(target_w, target_h, FilterType::Triangle)
                .to_rgba8()
        } else {
            rgba
        };

        let (fw, fh) = final_rgba.dimensions();
        let pixels = final_rgba.into_raw();
        let next_total = accumulated_bytes.saturating_add(pixels.len());
        if !frames.is_empty() && next_total > limits.max_decoded_bytes_per_item {
            break;
        }
        accumulated_bytes = next_total;

        frames.push(RgbaFrame {
            width: fw,
            height: fh,
            delay_ms: normalized_delay,
            pixels,
        });
    }

    if frames.is_empty() {
        return Err(TermError::MediaDecode(format!(
            "{format_label} contained zero frames"
        )));
    }

    Ok(DecodedMedia::from_rgba_frames(
        key,
        title,
        source_width,
        source_height,
        frames,
    ))
}

fn decode_static_bytes(
    key: String,
    bytes: &[u8],
    format: ImageFormat,
    title: Option<String>,
    limits: &DecodeLimits,
) -> TermResult<DecodedMedia> {
    let reader = ImageReader::with_format(Cursor::new(bytes), format);
    let (source_width, source_height) = reader
        .into_dimensions()
        .map_err(|err| TermError::MediaDecode(format!("invalid image header: {err}")))?;

    if source_width == 0 || source_height == 0 {
        return Err(TermError::MediaDecode(
            "image has zero width or height".to_owned(),
        ));
    }
    if source_width > limits.max_header_dimension || source_height > limits.max_header_dimension {
        return Err(TermError::MediaDecode(format!(
            "image dimensions {source_width}x{source_height} exceed safety limit {}",
            limits.max_header_dimension
        )));
    }

    let decoded = ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .map_err(|err| TermError::MediaDecode(format!("failed to decode {format:?}: {err}")))?;

    let (target_w, target_h) = aspect_clamped_pixels(
        source_width,
        source_height,
        limits.max_static_dimension,
        limits.max_static_dimension,
    );

    let rgba = if target_w != source_width || target_h != source_height {
        decoded
            .resize_exact(target_w, target_h, FilterType::Triangle)
            .to_rgba8()
    } else {
        decoded.to_rgba8()
    };

    let (fw, fh) = rgba.dimensions();
    let pixels = rgba.into_raw();
    if pixels.len() > limits.max_decoded_bytes_per_item {
        return Err(TermError::MediaDecode(format!(
            "decoded image footprint ({} bytes) exceeds limit ({} bytes)",
            pixels.len(),
            limits.max_decoded_bytes_per_item
        )));
    }

    let frame = RgbaFrame {
        width: fw,
        height: fh,
        delay_ms: 0,
        pixels,
    };

    Ok(DecodedMedia::from_rgba_frames(
        key,
        title,
        source_width,
        source_height,
        vec![frame],
    ))
}

/// Outcome returned by [`MediaFrameCache::insert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheInsertOutcome {
    /// `true` if the item fit within the cache's byte budget and was stored.
    pub cached: bool,
    /// Keys evicted in least-recently-used order to keep memory within budget.
    pub evicted_keys: Vec<String>,
}

/// Byte-bounded LRU cache of decoded source [`DecodedMedia`] sequences (default `64 MiB`).
///
/// Storing source RGBA frames in RAM ensures `SIGWINCH` resizes rescale locally and NEVER
/// re-download media over the network.
#[derive(Debug)]
pub struct MediaFrameCache {
    entries: LruCache<String, Arc<DecodedMedia>>,
    total_bytes: usize,
    budget_bytes: usize,
}

impl MediaFrameCache {
    /// Create a cache with the specified decoded-byte budget (default [`DEFAULT_MEDIA_CACHE_BYTES`]).
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: LruCache::unbounded(),
            total_bytes: 0,
            budget_bytes,
        }
    }

    /// Create a cache with the default `64 MiB` budget.
    #[must_use]
    pub fn with_default_budget() -> Self {
        Self::new(DEFAULT_MEDIA_CACHE_BYTES)
    }

    /// Insert a decoded media item, evicting least-recently-used entries until `total_bytes <= budget_bytes`.
    pub fn insert(&mut self, media: Arc<DecodedMedia>) -> CacheInsertOutcome {
        let mut evicted_keys = Vec::new();
        let incoming_bytes = media.byte_size;

        if incoming_bytes > self.budget_bytes {
            return CacheInsertOutcome {
                cached: false,
                evicted_keys,
            };
        }

        if let Some(prev) = self.entries.pop(&media.key) {
            self.total_bytes = self.total_bytes.saturating_sub(prev.byte_size);
        }

        while self.total_bytes.saturating_add(incoming_bytes) > self.budget_bytes {
            let Some((evicted_key, evicted_val)) = self.entries.pop_lru() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(evicted_val.byte_size);
            evicted_keys.push(evicted_key);
        }

        self.total_bytes = self.total_bytes.saturating_add(incoming_bytes);
        self.entries.put(media.key.clone(), media);

        CacheInsertOutcome {
            cached: true,
            evicted_keys,
        }
    }

    /// Fetch a cached [`DecodedMedia`] and promote it to most-recently-used.
    pub fn get(&mut self, key: &str) -> Option<Arc<DecodedMedia>> {
        self.entries.get(key).cloned()
    }

    /// Inspect a cached [`DecodedMedia`] without changing LRU recency order.
    #[must_use]
    pub fn peek(&self, key: &str) -> Option<&Arc<DecodedMedia>> {
        self.entries.peek(key)
    }

    /// Remove a specific key from the cache.
    pub fn remove(&mut self, key: &str) -> Option<Arc<DecodedMedia>> {
        let removed = self.entries.pop(key)?;
        self.total_bytes = self.total_bytes.saturating_sub(removed.byte_size);
        Some(removed)
    }

    /// Clear all cached items and return their keys.
    pub fn clear(&mut self) -> Vec<String> {
        let mut keys = Vec::with_capacity(self.entries.len());
        while let Some((k, _)) = self.entries.pop_lru() {
            keys.push(k);
        }
        self.total_bytes = 0;
        keys
    }

    /// Current total tracked bytes stored in the cache.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Configured maximum byte budget.
    #[must_use]
    pub const fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// Number of media entries currently resident in the cache.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no media items are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for MediaFrameCache {
    fn default() -> Self {
        Self::with_default_budget()
    }
}

/// Bounded asynchronous background image/GIF decoder pool backed by a shared [`MediaFrameCache`].
#[derive(Clone, Debug)]
pub struct MediaDecoderPool {
    semaphore: Arc<Semaphore>,
    cache: Arc<Mutex<MediaFrameCache>>,
    limits: DecodeLimits,
}

impl MediaDecoderPool {
    /// Create a decoder pool with explicit concurrency cap, cache byte budget, and decode limits.
    #[must_use]
    pub fn new(max_concurrent_decodes: usize, cache_budget_bytes: usize, limits: DecodeLimits) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent_decodes.max(1))),
            cache: Arc::new(Mutex::new(MediaFrameCache::new(cache_budget_bytes))),
            limits,
        }
    }

    /// Create a decoder pool with default settings (`8` workers, `64 MiB` cache).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(
            DEFAULT_DECODE_CONCURRENCY,
            DEFAULT_MEDIA_CACHE_BYTES,
            DecodeLimits::default(),
        )
    }

    /// Check the in-RAM LRU cache synchronously without touching the network or spawning a task.
    #[must_use]
    pub fn get_cached(&self, key: &str) -> Option<Arc<DecodedMedia>> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key)
    }

    /// Insert pre-decoded media (e.g. offline mock frames) directly into the shared cache.
    pub fn insert_cached(&self, media: Arc<DecodedMedia>) -> CacheInsertOutcome {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(media)
    }

    /// Current cached byte usage.
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.total_bytes()
    }

    /// Decode `bytes` on a bounded background blocking thread and store the result in the LRU cache.
    ///
    /// Returns immediately on cache hit without re-decoding.
    pub async fn decode_and_cache(
        &self,
        key: String,
        bytes: Vec<u8>,
        title: Option<String>,
    ) -> TermResult<(Arc<DecodedMedia>, Vec<String>)> {
        if let Some(existing) = self.get_cached(&key) {
            return Ok((existing, Vec::new()));
        }

        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| TermError::MediaDecode("decoder pool closed".to_owned()))?;

        // Re-check after waiting for semaphore permit in case a sibling task decoded the same key.
        if let Some(existing) = self.get_cached(&key) {
            return Ok((existing, Vec::new()));
        }

        let limits = self.limits;
        let key_for_decode = key.clone();
        let decoded = tokio::task::spawn_blocking(move || {
            decode_image_bytes(key_for_decode, &bytes, title, &limits)
        })
        .await
        .map_err(|err| TermError::TaskJoin(err.to_string()))??;

        let arc = Arc::new(decoded);
        let outcome = self.insert_cached(Arc::clone(&arc));
        Ok((arc, outcome.evicted_keys))
    }
}

fn zlib_compress_fast(raw: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(raw)?;
    encoder.finish()
}

/// Direct Kitty graphics protocol frame (`a=T`, no Unicode placeholders) for WezTerm / Konsole.
#[derive(Debug, Clone)]
pub struct KittyDirectFrame {
    /// Assigned Kitty image ID (`> 0`).
    pub id: u32,
    /// Rendered cell bounding box.
    pub size: Size,
    /// Encoded escape-sequence payload ready to write to the top-left cell.
    pub payload: String,
}

impl KittyDirectFrame {
    /// Encode `image` into chunked Kitty direct graphics APC sequences (`f=32,o=z` fast zlib-deflated RGBA8).
    #[must_use]
    pub fn encode(image: &DynamicImage, size: Size, id: u32, is_tmux: bool) -> Self {
        let rgba = image.to_rgba8();
        let (px_w, px_h) = rgba.dimensions();
        let raw = rgba.as_raw();
        let (payload_bytes, compression_flag): (Cow<'_, [u8]>, &str) = match zlib_compress_fast(raw)
        {
            Ok(compressed) => (Cow::Owned(compressed), "o=z,"),
            Err(_) => (Cow::Borrowed(raw), ""),
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload_bytes.as_ref());

        let mut payload = String::with_capacity(b64.len() + 128);
        let chunks: Vec<&str> = b64
            .as_bytes()
            .chunks(KITTY_CHUNK_BYTES)
            .filter_map(|c| std::str::from_utf8(c).ok())
            .collect();

        let last_idx = chunks.len().saturating_sub(1);
        for (idx, chunk) in chunks.iter().enumerate() {
            let more = u8::from(idx < last_idx);
            let esc = if is_tmux { "\x1b\x1b" } else { "\x1b" };
            if is_tmux {
                payload.push_str("\x1bPtmux;");
            }
            if idx == 0 {
                // `C=1` suppresses terminal cursor advance so `ratatui`'s `ForcedWidth(1)` cursor
                // tracking remains synchronized on WezTerm and Konsole. `s`/`v` carry uncompressed
                // pixel dimensions even when `o=z` deflates the payload stream.
                let _ = write!(
                    payload,
                    "{esc}_Ga=T,f=32,{compression_flag}s={px_w},v={px_h},c={},r={},i={id},C=1,q=2,m={more};{chunk}{esc}\\",
                    size.width, size.height
                );
            } else {
                let _ = write!(payload, "{esc}_Gm={more};{chunk}{esc}\\");
            }
            if is_tmux {
                payload.push_str("\x1b\\");
            }
        }

        Self { id, size, payload }
    }

    /// Render the direct Kitty graphics payload into `buf`, marking covered cells `Skip`.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 || self.size.width == 0 || self.size.height == 0 {
            return;
        }
        let draw_w = area.width.min(self.size.width);
        let draw_h = area.height.min(self.size.height);
        let render_area = Rect::new(area.x, area.y, draw_w, draw_h);

        if let Some(cell) = buf.cell_mut(Position::from(render_area)) {
            cell.set_symbol(&self.payload)
                .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
        }

        for y in render_area.top()..render_area.bottom() {
            for x in render_area.left()..render_area.right() {
                if x == render_area.left() && y == render_area.top() {
                    continue;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_diff_option(CellDiffOption::Skip);
                }
            }
        }
    }
}

/// Pure text fallback frame (`[GIF: title]` / `[IMG: WxH]`) for `TextTagOnly` / `16×2` / ASCII.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTagFrame {
    /// Formatted tag label.
    pub label: String,
    /// Rendered 1-row cell size.
    pub size: Size,
}

impl TextTagFrame {
    /// Format a clean `[GIF: title]` or `[IMG: WxH]` tag clamped to `max_cols`.
    #[must_use]
    pub fn new(media: &DecodedMedia, charset: ResolvedCharset, max_cols: u16) -> Self {
        Self::from_parts(
            media.title.as_deref(),
            media.is_animated,
            media.source_width,
            media.source_height,
            charset,
            max_cols,
        )
    }

    /// Format a clean `[GIF: title]` or `[IMG: WxH]` tag from explicit metadata clamped to `max_cols`.
    #[must_use]
    pub fn from_parts(
        title: Option<&str>,
        is_animated: bool,
        source_width: u32,
        source_height: u32,
        charset: ResolvedCharset,
        max_cols: u16,
    ) -> Self {
        let raw = if is_animated {
            let title = title
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("animation");
            format!("[GIF: {}]", sanitize_tag_text(title, charset))
        } else if let Some(title) = title.map(str::trim).filter(|s| !s.is_empty()) {
            let clean = sanitize_tag_text(title, charset);
            let compact = format!("[IMG: {source_width}x{source_height}]");
            let verbose = format!("[IMG: {clean} {source_width}x{source_height}]");
            if display_width(&verbose) <= usize::from(max_cols) {
                verbose
            } else {
                compact
            }
        } else {
            format!("[IMG: {source_width}x{source_height}]")
        };

        let clipped = clip_to_cells(&raw, usize::from(max_cols));
        let width = (display_width(&clipped) as u16).min(max_cols);
        Self {
            label: clipped,
            size: Size::new(width, u16::from(max_cols > 0)),
        }
    }

    /// Render the text tag cleanly onto the top row of `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let clipped = clip_to_cells(&self.label, usize::from(area.width));
        let mut col = area.x;
        let max_x = area.x.saturating_add(area.width);
        for ch in clipped.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
            if w == 0 {
                continue;
            }
            if col.saturating_add(w) > max_x {
                break;
            }
            if let Some(cell) = buf.cell_mut((col, area.y)) {
                let mut tmp = [0u8; 4];
                cell.set_symbol(ch.encode_utf8(&mut tmp));
            }
            col = col.saturating_add(w);
        }
    }
}

fn sanitize_tag_text(raw: &str, charset: ResolvedCharset) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_control() {
            continue;
        }
        if charset == ResolvedCharset::Ascii7Bit {
            let code = ch as u32;
            if (0x20..=0x7D).contains(&code) && ch != '\\' {
                out.push(ch);
            } else {
                out.push('?');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

fn clip_to_cells(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(s.len().min(max_cells));
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used.saturating_add(w) > max_cells {
            break;
        }
        out.push(ch);
        used = used.saturating_add(w);
    }
    out
}

/// Encoded terminal media frame ready to render into a `ratatui` buffer across any [`GraphicsTier`].
#[derive(Clone)]
pub enum RenderedMedia {
    /// Encoded by `ratatui-image` (`KittyPlaceholder`, `Sixel`, `ITerm2`, or `HalfBlock`).
    Ratatui {
        /// Underlying `ratatui-image` fixed protocol state.
        protocol: Protocol,
        /// Active graphics tier that produced this frame.
        tier: GraphicsTier,
        /// Assigned Kitty image ID when `tier == GraphicsTier::KittyPlaceholder`.
        kitty_id: Option<u32>,
        /// Approximate serialized byte cost for PTY bandwidth accounting.
        estimated_bytes: usize,
    },
    /// Encoded by [`KittyDirectFrame`] (`WezTerm` / `Konsole`).
    KittyDirect(KittyDirectFrame),
    /// Encoded as a pure text placeholder (`[GIF: title]` / `[IMG: WxH]`).
    TextTag(TextTagFrame),
}

impl RenderedMedia {
    /// Rendered bounding box in terminal character cells.
    #[must_use]
    pub fn size(&self) -> Size {
        match self {
            Self::Ratatui { protocol, .. } => protocol.size(),
            Self::KittyDirect(frame) => frame.size,
            Self::TextTag(frame) => frame.size,
        }
    }

    /// Graphics tier used to produce this rendered frame.
    #[must_use]
    pub const fn tier(&self) -> GraphicsTier {
        match self {
            Self::Ratatui { tier, .. } => *tier,
            Self::KittyDirect(_) => GraphicsTier::KittyDirect,
            Self::TextTag(_) => GraphicsTier::TextTagOnly,
        }
    }

    /// Assigned Kitty image ID, if applicable (for `a=d,d=I,i=<id>` eviction cleanup).
    #[must_use]
    pub const fn kitty_id(&self) -> Option<u32> {
        match self {
            Self::Ratatui { kitty_id, .. } => *kitty_id,
            Self::KittyDirect(frame) => Some(frame.id),
            Self::TextTag(_) => None,
        }
    }

    /// Estimated PTY byte size emitted when rendering this frame.
    #[must_use]
    pub fn estimated_pty_bytes(&self) -> usize {
        match self {
            Self::Ratatui { estimated_bytes, .. } => *estimated_bytes,
            Self::KittyDirect(frame) => frame.payload.len(),
            Self::TextTag(frame) => frame.label.len(),
        }
    }

    /// Render this frame into `buf` clipped to `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        match self {
            Self::Ratatui { protocol, .. } => {
                RatatuiImageWidget::new(protocol)
                    .allow_clipping(true)
                    .render(area, buf);
            }
            Self::KittyDirect(frame) => frame.render(area, buf),
            Self::TextTag(frame) => frame.render(area, buf),
        }
    }
}

/// Stateless `ratatui` widget borrowing a [`RenderedMedia`] for zero-cost immediate rendering.
pub struct MediaWidget<'a> {
    media: &'a RenderedMedia,
}

impl<'a> MediaWidget<'a> {
    /// Wrap a [`RenderedMedia`] as a `ratatui` [`Widget`].
    #[must_use]
    pub const fn new(media: &'a RenderedMedia) -> Self {
        Self { media }
    }
}

impl Widget for MediaWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.media.render(area, buf);
    }
}

/// Multi-tier renderer bridging `bbs-core` [`GraphicsTier`], `ratatui-image` [`Picker`]/protocols,
/// `KittyDirect`, and `TextTagOnly`.
pub struct MediaRenderer {
    tier: GraphicsTier,
    charset: ResolvedCharset,
    cell_pixels: (u16, u16),
    tmux_passthrough: bool,
    picker: Picker,
    next_kitty_id: AtomicU32,
}

impl MediaRenderer {
    /// Construct a [`MediaRenderer`] directly from a [`ProbeReport`].
    #[must_use]
    pub fn from_probe(report: &ProbeReport) -> Self {
        Self::new(
            report.capabilities.graphics_tier,
            report.resolved_charset,
            report.cell_pixels,
            report.tmux_passthrough_active,
        )
    }

    /// Construct a [`MediaRenderer`] with explicit tier, charset, cell pixel metrics, and tmux mode.
    #[must_use]
    pub fn new(
        tier: GraphicsTier,
        charset: ResolvedCharset,
        cell_pixels: (u16, u16),
        tmux_passthrough: bool,
    ) -> Self {
        let cell_pixels = (cell_pixels.0.max(1), cell_pixels.1.max(1));
        let mut picker = Picker::halfblocks();
        let proto_type = match tier {
            GraphicsTier::KittyPlaceholder => ProtocolType::Kitty,
            GraphicsTier::Sixel => ProtocolType::Sixel,
            GraphicsTier::ITerm2 => ProtocolType::Iterm2,
            // `ratatui-image` Kitty uses `U=1` Unicode placeholders unsupported by KittyDirect
            // terminals (WezTerm/Konsole); keep Picker on Halfblocks and render KittyDirect via
            // `MediaRenderer::render_frame` (`KittyDirectFrame`).
            GraphicsTier::KittyDirect | GraphicsTier::HalfBlock | GraphicsTier::TextTagOnly => {
                ProtocolType::Halfblocks
            }
        };
        picker.set_protocol_type(proto_type);

        Self {
            tier,
            charset,
            cell_pixels,
            tmux_passthrough,
            picker,
            // Start above probe ID 31 so rendered image IDs never collide with probe responses.
            next_kitty_id: AtomicU32::new(100),
        }
    }

    /// Active [`GraphicsTier`].
    #[must_use]
    pub const fn tier(&self) -> GraphicsTier {
        self.tier
    }

    /// Update active cell pixel dimensions (for example after a `SIGWINCH` font-zoom resize).
    pub fn set_cell_pixels(&mut self, cell_pixels: (u16, u16)) {
        self.cell_pixels = (
            cell_pixels.0.max(1),
            cell_pixels.1.max(1),
        );
    }

    /// Current character cell pixel dimensions `(width_px, height_px)`.
    #[must_use]
    pub const fn cell_pixels(&self) -> (u16, u16) {
        self.cell_pixels
    }

    /// Borrow the underlying configured `ratatui-image` [`Picker`].
    #[must_use]
    pub const fn picker(&self) -> &Picker {
        &self.picker
    }

    /// Allocate a monotonic non-zero Kitty image ID (`>= 100`).
    fn alloc_kitty_id(&self) -> u32 {
        let id = self.next_kitty_id.fetch_add(1, Ordering::Relaxed);
        if id < 100 {
            self.next_kitty_id.store(101, Ordering::Relaxed);
            100
        } else {
            id
        }
    }

    /// Build a `ratatui-image` [`StatefulProtocol`] configured with our exact probed [`FontSize`]
    /// and `tmux` passthrough settings.
    ///
    /// Returns [`TermError::MediaEncode`] for [`GraphicsTier::KittyDirect`] because `ratatui-image`'s
    /// `StatefulKitty` unconditionally emits `U=1` (`U+10EEEE`) Unicode placeholders unsupported by
    /// WezTerm and Konsole — callers on `KittyDirect` should use [`Self::render_frame`] (which emits
    /// [`KittyDirectFrame`]) or [`Self::render_halfblock_proxy`].
    pub fn create_stateful_protocol(&self, frame: &RgbaFrame) -> TermResult<StatefulProtocol> {
        if self.tier == GraphicsTier::KittyDirect {
            return Err(TermError::MediaEncode(
                "KittyDirect does not support ratatui-image StatefulKitty (U=1 Unicode placeholders); use MediaRenderer::render_frame or render_halfblock_proxy instead"
                    .to_owned(),
            ));
        }

        let image = rgba_frame_to_dynamic_image(frame)?;
        let font_size = FontSize::new(self.cell_pixels.0, self.cell_pixels.1);
        let proto_type = match self.tier {
            GraphicsTier::KittyPlaceholder => StatefulProtocolType::Kitty(StatefulKitty::new(
                self.alloc_kitty_id(),
                self.tmux_passthrough,
                true,
            )),
            GraphicsTier::Sixel => StatefulProtocolType::Sixel(Sixel {
                is_tmux: self.tmux_passthrough,
                ..Sixel::default()
            }),
            GraphicsTier::ITerm2 => StatefulProtocolType::ITerm2(Iterm2 {
                is_tmux: self.tmux_passthrough,
                ..Iterm2::default()
            }),
            GraphicsTier::HalfBlock | GraphicsTier::TextTagOnly => {
                StatefulProtocolType::Halfblocks(Halfblocks::default())
            }
            GraphicsTier::KittyDirect => {
                return Err(TermError::MediaEncode(
                    "KittyDirect is unsupported by StatefulProtocol".to_owned(),
                ));
            }
        };
        Ok(StatefulProtocol::new(image, font_size, None, proto_type))
    }

    /// Render a text-tag placeholder (`[GIF: title]` / `[IMG: WxH]`) for any media item.
    #[must_use]
    pub fn render_text_tag(&self, media: &DecodedMedia, max_cols: u16) -> RenderedMedia {
        RenderedMedia::TextTag(TextTagFrame::new(media, self.charset, max_cols))
    }

    /// Render a lightweight `HalfBlock` proxy (used during active scrolling or `100 ms` `SIGWINCH`
    /// resize settling on Sixel/iTerm2/KittyDirect terminals).
    pub fn render_halfblock_proxy(
        &self,
        media: &DecodedMedia,
        frame_index: usize,
        max_cols: u16,
        max_rows: u16,
        fit: MediaFitMode,
    ) -> TermResult<RenderedMedia> {
        if self.tier == GraphicsTier::TextTagOnly || self.charset == ResolvedCharset::Ascii7Bit {
            return Ok(self.render_text_tag(media, max_cols));
        }
        self.render_frame_with_tier(
            media,
            frame_index,
            max_cols,
            max_rows,
            fit,
            GraphicsTier::HalfBlock,
        )
    }

    /// Encode and render `media.frames[frame_index]` for the active [`GraphicsTier`].
    pub fn render_frame(
        &self,
        media: &DecodedMedia,
        frame_index: usize,
        max_cols: u16,
        max_rows: u16,
        fit: MediaFitMode,
    ) -> TermResult<RenderedMedia> {
        self.render_frame_with_tier(media, frame_index, max_cols, max_rows, fit, self.tier)
    }

    fn render_frame_with_tier(
        &self,
        media: &DecodedMedia,
        frame_index: usize,
        max_cols: u16,
        max_rows: u16,
        fit: MediaFitMode,
        tier: GraphicsTier,
    ) -> TermResult<RenderedMedia> {
        let frame = media
            .frame(frame_index)
            .ok_or_else(|| TermError::MediaDecode("media has no decoded frames".to_owned()))?;

        encode_rgba_frame(
            frame,
            media.title.as_deref(),
            media.is_animated,
            (media.source_width, media.source_height),
            max_cols,
            max_rows,
            fit,
            tier,
            self.charset,
            self.cell_pixels,
            self.tmux_passthrough,
            &|| self.alloc_kitty_id(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_rgba_frame(
    frame: &RgbaFrame,
    title: Option<&str>,
    is_animated: bool,
    source_dims: (u32, u32),
    max_cols: u16,
    max_rows: u16,
    fit: MediaFitMode,
    tier: GraphicsTier,
    charset: ResolvedCharset,
    cell_pixels: (u16, u16),
    tmux_passthrough: bool,
    alloc_kitty_id: &impl Fn() -> u32,
) -> TermResult<RenderedMedia> {
    if max_cols == 0 || max_rows == 0 {
        return Ok(RenderedMedia::TextTag(TextTagFrame {
            label: String::new(),
            size: Size::new(0, 0),
        }));
    }

    if tier == GraphicsTier::TextTagOnly || charset == ResolvedCharset::Ascii7Bit {
        return Ok(RenderedMedia::TextTag(TextTagFrame::from_parts(
            title,
            is_animated,
            source_dims.0,
            source_dims.1,
            charset,
            max_cols,
        )));
    }

    let safe_cell_px = (cell_pixels.0.max(1), cell_pixels.1.max(1));
    let cell_box = compute_image_cell_box(
        frame.width,
        frame.height,
        max_cols,
        max_rows,
        safe_cell_px,
        fit,
    );
    if cell_box.width == 0 || cell_box.height == 0 {
        return Ok(RenderedMedia::TextTag(TextTagFrame::from_parts(
            title,
            is_animated,
            source_dims.0,
            source_dims.1,
            charset,
            max_cols,
        )));
    }

    let image = rgba_frame_to_dynamic_image(frame)?;
    let target_px_w = (u32::from(cell_box.width) * u32::from(safe_cell_px.0)).max(1);
    let target_px_h = (u32::from(cell_box.height) * u32::from(safe_cell_px.1)).max(1);
    let resized = if image.width() == target_px_w && image.height() == target_px_h {
        image
    } else {
        image.resize_exact(target_px_w, target_px_h, FilterType::Triangle)
    };

    match tier {
        GraphicsTier::KittyPlaceholder => {
            let id = alloc_kitty_id();
            // `Kitty::new(..., true)` deflates RGBA8 with fast zlib (`o=z`), shrinking typical
            // wire payloads 3x-20x; estimate conservatively at 50% of raw base64 size.
            let raw_b64 = (target_px_w as usize * target_px_h as usize * 4) * 4 / 3;
            let est = (raw_b64 / 2).max(256);
            let proto = Kitty::new(resized, cell_box, id, tmux_passthrough, true)
                .map_err(|err| TermError::MediaEncode(err.to_string()))?;
            Ok(RenderedMedia::Ratatui {
                protocol: Protocol::Kitty(proto),
                tier: GraphicsTier::KittyPlaceholder,
                kitty_id: Some(id),
                estimated_bytes: est,
            })
        }
        GraphicsTier::KittyDirect => {
            let id = alloc_kitty_id();
            Ok(RenderedMedia::KittyDirect(KittyDirectFrame::encode(
                &resized,
                cell_box,
                id,
                tmux_passthrough,
            )))
        }
        GraphicsTier::Sixel => {
            let proto = Sixel::new(resized, cell_box, tmux_passthrough)
                .map_err(|err| TermError::MediaEncode(err.to_string()))?;
            let est = proto.data.len();
            Ok(RenderedMedia::Ratatui {
                protocol: Protocol::Sixel(proto),
                tier: GraphicsTier::Sixel,
                kitty_id: None,
                estimated_bytes: est,
            })
        }
        GraphicsTier::ITerm2 => {
            let proto = Iterm2::new(resized, cell_box, tmux_passthrough)
                .map_err(|err| TermError::MediaEncode(err.to_string()))?;
            let est = proto.data.len();
            Ok(RenderedMedia::Ratatui {
                protocol: Protocol::ITerm2(proto),
                tier: GraphicsTier::ITerm2,
                kitty_id: None,
                estimated_bytes: est,
            })
        }
        GraphicsTier::HalfBlock => {
            let proto = Halfblocks::new(resized, cell_box)
                .map_err(|err| TermError::MediaEncode(err.to_string()))?;
            // ~24 bytes SGR + UTF-8 '▀' per cell worst case.
            let est = usize::from(cell_box.width) * usize::from(cell_box.height) * 24;
            Ok(RenderedMedia::Ratatui {
                protocol: Protocol::Halfblocks(proto),
                tier: GraphicsTier::HalfBlock,
                kitty_id: None,
                estimated_bytes: est,
            })
        }
        GraphicsTier::TextTagOnly => Ok(RenderedMedia::TextTag(TextTagFrame::from_parts(
            title,
            is_animated,
            source_dims.0,
            source_dims.1,
            charset,
            max_cols,
        ))),
    }
}

/// Request submitted to [`MediaEncoderPool`] for non-blocking off-main-thread graphics encoding.
#[derive(Debug, Clone)]
pub struct MediaEncodeRequest {
    /// Caller correlation/cache key (e.g. canonical URL or composite slot key).
    pub key: String,
    /// Shared decoded RGBA8 frame pixels.
    pub frame: Arc<RgbaFrame>,
    /// Target bounding box width in terminal character cells.
    pub width_cells: u16,
    /// Target bounding box height in terminal character cells.
    pub height_cells: u16,
    /// Graphics protocol tier used to encode this frame.
    pub tier: GraphicsTier,
    /// Terminal character cell dimensions `(width_px, height_px)`.
    pub cell_px: (u16, u16),
    /// Aspect-ratio fit mode (default [`MediaFitMode::Contain`]).
    pub fit: MediaFitMode,
    /// Active character set used when falling back to `TextTagOnly`.
    pub charset: ResolvedCharset,
    /// Wrap Kitty/Sixel/iTerm2 sequences in `tmux` DCS passthrough.
    pub tmux_passthrough: bool,
    /// Optional label/title for `TextTagOnly` fallback rendering.
    pub title: Option<String>,
    /// Whether this frame belongs to an animated sequence (for bandwidth prioritization).
    pub is_animated: bool,
}

impl MediaEncodeRequest {
    /// Construct an encode request with standard defaults (`Contain`, `Utf8`, no tmux passthrough).
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        frame: Arc<RgbaFrame>,
        width_cells: u16,
        height_cells: u16,
        tier: GraphicsTier,
        cell_px: (u16, u16),
    ) -> Self {
        Self {
            key: key.into(),
            frame,
            width_cells,
            height_cells,
            tier,
            cell_px,
            fit: MediaFitMode::Contain,
            charset: ResolvedCharset::Utf8,
            tmux_passthrough: false,
            title: None,
            is_animated: false,
        }
    }
}

/// Ready-to-draw encoded terminal graphics frame returned asynchronously by [`MediaEncoderPool`].
#[derive(Clone)]
pub struct RenderedMediaFrame {
    /// Caller correlation/cache key echoed from the request.
    pub key: String,
    /// Requested bounding box width in terminal character cells.
    pub width_cells: u16,
    /// Requested bounding box height in terminal character cells.
    pub height_cells: u16,
    /// Graphics tier that produced [`Self::media`].
    pub tier: GraphicsTier,
    /// Whether this frame belongs to an animated sequence.
    pub is_animated: bool,
    /// Encoded terminal graphics payload ready to render immediately into a `ratatui` buffer.
    pub media: RenderedMedia,
}

impl RenderedMediaFrame {
    /// Render this frame into `buf` clipped to `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        self.media.render(area, buf);
    }

    /// Estimated PTY byte footprint emitted when rendering this frame.
    #[must_use]
    pub fn estimated_pty_bytes(&self) -> usize {
        self.media.estimated_pty_bytes()
    }

    /// Assigned Kitty image ID, if applicable.
    #[must_use]
    pub const fn kitty_id(&self) -> Option<u32> {
        self.media.kitty_id()
    }

    /// Consume this wrapper and return the inner [`RenderedMedia`].
    #[must_use]
    pub fn into_media(self) -> RenderedMedia {
        self.media
    }
}

type EncodeJobKey = (String, u16, u16, GraphicsTier);

struct InFlightEncodeGuard {
    in_flight: Arc<Mutex<HashSet<EncodeJobKey>>>,
    job_key: EncodeJobKey,
}

impl Drop for InFlightEncodeGuard {
    fn drop(&mut self) {
        let mut guard = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&self.job_key);
    }
}

/// Bounded asynchronous off-main-thread pixel encoder (`spawn_blocking` + `mpsc` result stream).
///
/// Keeps `terminal.draw(...)` completely free of RGBA cloning, resizing, zlib deflation, Sixel
/// quantization, and PNG encoding stalls by dispatching encoding onto Tokio's blocking pool and
/// delivering ready-to-draw [`RenderedMediaFrame`]s over an `mpsc` channel suitable for
/// `tokio::select!`.
#[derive(Clone)]
pub struct MediaEncoderPool {
    semaphore: Arc<Semaphore>,
    tx: mpsc::UnboundedSender<RenderedMediaFrame>,
    in_flight: Arc<Mutex<HashSet<EncodeJobKey>>>,
    next_kitty_id: Arc<AtomicU32>,
    charset: ResolvedCharset,
    tmux_passthrough: bool,
}

impl MediaEncoderPool {
    /// Create a bounded encoder pool with explicit concurrency cap, charset, and tmux mode,
    /// returning `(pool, result_rx)` for direct `tokio::select!` integration.
    #[must_use]
    pub fn with_options(
        max_concurrent_encodes: usize,
        charset: ResolvedCharset,
        tmux_passthrough: bool,
    ) -> (Self, mpsc::UnboundedReceiver<RenderedMediaFrame>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let pool = Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent_encodes.max(1))),
            tx,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            // Start above Kitty probe ID 31 and well separated from MediaRenderer's 100.. range.
            next_kitty_id: Arc::new(AtomicU32::new(10_000)),
            charset,
            tmux_passthrough,
        };
        (pool, rx)
    }

    /// Create an encoder pool with explicit concurrency cap and default UTF-8 / non-tmux settings.
    #[must_use]
    pub fn new(
        max_concurrent_encodes: usize,
    ) -> (Self, mpsc::UnboundedReceiver<RenderedMediaFrame>) {
        Self::with_options(max_concurrent_encodes, ResolvedCharset::Utf8, false)
    }

    /// Create an encoder pool configured with default concurrency (`4` workers).
    #[must_use]
    pub fn with_defaults() -> (Self, mpsc::UnboundedReceiver<RenderedMediaFrame>) {
        Self::new(DEFAULT_ENCODE_CONCURRENCY)
    }

    /// Create an encoder pool inheriting `resolved_charset` and `tmux_passthrough_active` from a [`ProbeReport`].
    #[must_use]
    pub fn from_probe(
        report: &ProbeReport,
    ) -> (Self, mpsc::UnboundedReceiver<RenderedMediaFrame>) {
        Self::with_options(
            DEFAULT_ENCODE_CONCURRENCY,
            report.resolved_charset,
            report.tmux_passthrough_active,
        )
    }

    fn alloc_kitty_id(counter: &AtomicU32) -> u32 {
        let id = counter.fetch_add(1, Ordering::Relaxed);
        if id < 10_000 {
            counter.store(10_001, Ordering::Relaxed);
            10_000
        } else {
            id
        }
    }

    /// Enqueue a non-blocking background encode job from positional parameters.
    ///
    /// Returns `true` if a new background task was spawned, or `false` if an identical
    /// `(key, width_cells, height_cells, tier)` job is already in flight.
    pub fn enqueue(
        &self,
        key: impl Into<String>,
        frame: Arc<RgbaFrame>,
        width_cells: u16,
        height_cells: u16,
        tier: GraphicsTier,
        cell_px: (u16, u16),
    ) -> bool {
        let mut req = MediaEncodeRequest::new(key, frame, width_cells, height_cells, tier, cell_px);
        req.charset = self.charset;
        req.tmux_passthrough = self.tmux_passthrough;
        self.enqueue_request(req)
    }

    /// Enqueue a full [`MediaEncodeRequest`] onto the bounded background blocking pool.
    ///
    /// Deduplicates concurrent jobs with the same `(key, width_cells, height_cells, tier)` and
    /// delivers completed [`RenderedMediaFrame`]s on the paired `mpsc` receiver.
    pub fn enqueue_request(&self, req: MediaEncodeRequest) -> bool {
        let job_key: EncodeJobKey = (req.key.clone(), req.width_cells, req.height_cells, req.tier);
        {
            let mut guard = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !guard.insert(job_key.clone()) {
                return false;
            }
        }

        let semaphore = Arc::clone(&self.semaphore);
        let tx = self.tx.clone();
        let in_flight = Arc::clone(&self.in_flight);
        let kitty_ids = Arc::clone(&self.next_kitty_id);

        tokio::spawn(async move {
            let _guard = InFlightEncodeGuard { in_flight, job_key };
            let Ok(_permit) = semaphore.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || encode_request_sync(req, &kitty_ids))
                .await;
            match result {
                Ok(Ok(rendered_frame)) => {
                    let _ = tx.send(rendered_frame);
                }
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, "background media encode failed");
                }
                Err(err) => {
                    tracing::warn!(error = %err, "background media encode task join failed");
                }
            }
        });
        true
    }

    /// Encode a single [`MediaEncodeRequest`] asynchronously on the bounded blocking pool and
    /// return the ready-to-draw [`RenderedMediaFrame`] directly.
    pub async fn encode_once(&self, req: MediaEncodeRequest) -> TermResult<RenderedMediaFrame> {
        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| TermError::MediaEncode("encoder pool closed".to_owned()))?;
        let kitty_ids = Arc::clone(&self.next_kitty_id);
        tokio::task::spawn_blocking(move || encode_request_sync(req, &kitty_ids))
            .await
            .map_err(|err| TermError::TaskJoin(err.to_string()))?
    }

    /// Returns `true` if an encode job for `(key, width_cells, height_cells, tier)` is currently in flight.
    #[must_use]
    pub fn is_in_flight(
        &self,
        key: &str,
        width_cells: u16,
        height_cells: u16,
        tier: GraphicsTier,
    ) -> bool {
        let guard = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.contains(&(key.to_owned(), width_cells, height_cells, tier))
    }

    /// Clear in-flight deduplication entries (e.g. after a terminal resize or redraw reset).
    pub fn clear_in_flight(&self) {
        let mut guard = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }
}

fn encode_request_sync(
    req: MediaEncodeRequest,
    kitty_ids: &AtomicU32,
) -> TermResult<RenderedMediaFrame> {
    let source_dims = (req.frame.width, req.frame.height);
    let media = encode_rgba_frame(
        &req.frame,
        req.title.as_deref(),
        req.is_animated,
        source_dims,
        req.width_cells,
        req.height_cells,
        req.fit,
        req.tier,
        req.charset,
        req.cell_px,
        req.tmux_passthrough,
        &|| MediaEncoderPool::alloc_kitty_id(kitty_ids),
    )?;
    Ok(RenderedMediaFrame {
        key: req.key,
        width_cells: req.width_cells,
        height_cells: req.height_cells,
        tier: req.tier,
        is_animated: req.is_animated,
        media,
    })
}

/// Sliding-window PTY byte-rate governor preventing animated media from saturating slow PTYs or
/// stalling keyboard input over SSH/tmux while guaranteeing static images are never permanently
/// starved via single-frame token-bucket debt.
#[derive(Debug, Clone)]
pub struct PtyBandwidthGovernor {
    budget_bytes_per_sec: usize,
    window: Duration,
    samples: VecDeque<(Instant, usize)>,
    window_bytes: usize,
}

impl PtyBandwidthGovernor {
    /// Construct a governor with a specific bytes-per-second cap.
    #[must_use]
    pub fn new(budget_bytes_per_sec: usize) -> Self {
        Self {
            budget_bytes_per_sec,
            window: Duration::from_secs(1),
            samples: VecDeque::new(),
            window_bytes: 0,
        }
    }

    /// Construct a governor tuned for local (`2 MiB/s`) or SSH (`128 KiB/s`) sessions.
    #[must_use]
    pub fn for_session(over_ssh: bool) -> Self {
        Self::new(if over_ssh {
            SSH_PTY_BYTE_BUDGET_PER_SEC
        } else {
            LOCAL_PTY_BYTE_BUDGET_PER_SEC
        })
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&(ts, bytes)) = self.samples.front() {
            if now.saturating_duration_since(ts) >= self.window {
                self.samples.pop_front();
                self.window_bytes = self.window_bytes.saturating_sub(bytes);
            } else {
                break;
            }
        }
    }

    /// Returns `true` if emitting a static image of `bytes` at `now` is allowed.
    ///
    /// Uses token-bucket debt: when `window_bytes == 0`, a single static image is ALWAYS permitted
    /// even if `bytes > budget_bytes_per_sec`, so large static images can never be permanently
    /// starved. Subsequent frames wait until the rolling 1-second window drains.
    pub fn can_emit_static(&mut self, bytes: usize, now: Instant) -> bool {
        self.prune(now);
        self.window_bytes == 0
            || self.window_bytes.saturating_add(bytes) <= self.budget_bytes_per_sec
    }

    /// Returns `true` if emitting an animated GIF or inline video frame of `bytes` at `now` stays
    /// within the animation sub-budget (`75%` of `budget_bytes_per_sec`).
    ///
    /// Animations never incur token-bucket debt and throttle before static images so large static
    /// attachments always render promptly.
    pub fn can_emit_animation(&mut self, bytes: usize, now: Instant) -> bool {
        self.prune(now);
        let animation_budget = (self.budget_bytes_per_sec / 4).saturating_mul(3);
        self.window_bytes.saturating_add(bytes) <= animation_budget
    }

    /// Dispatch bandwidth check based on whether the frame is animated (`true`) or static (`false`).
    pub fn can_emit_for(&mut self, bytes: usize, is_animated: bool, now: Instant) -> bool {
        if is_animated {
            self.can_emit_animation(bytes, now)
        } else {
            self.can_emit_static(bytes, now)
        }
    }

    /// Returns `true` if emitting a static frame of `bytes` at `now` is permitted (alias for
    /// [`Self::can_emit_static`], preserving single-frame debt so static images never starve).
    pub fn can_emit(&mut self, bytes: usize, now: Instant) -> bool {
        self.can_emit_static(bytes, now)
    }

    /// Record that `bytes` of media data were emitted at `now`.
    pub fn record_emitted(&mut self, bytes: usize, now: Instant) {
        self.prune(now);
        self.samples.push_back((now, bytes));
        self.window_bytes = self.window_bytes.saturating_add(bytes);
    }

    /// Current bytes emitted inside the rolling 1-second window.
    pub fn current_rate_bytes(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.window_bytes
    }

    /// Configured bytes-per-second ceiling.
    #[must_use]
    pub const fn budget_bytes_per_sec(&self) -> usize {
        self.budget_bytes_per_sec
    }
}

impl Default for PtyBandwidthGovernor {
    fn default() -> Self {
        Self::for_session(false)
    }
}

/// Default cell pixel dimensions helper re-exported for callers.
#[must_use]
pub const fn default_cell_pixels() -> (u16, u16) {
    DEFAULT_CELL_PIXELS
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, Rgba};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_gif(frame_count: usize, width: u32, height: u32, delay_ms: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut buf);
            assert!(encoder.set_repeat(Repeat::Infinite).is_ok());
            for idx in 0..frame_count {
                let color = Rgba([
                    ((idx * 60) % 255) as u8,
                    ((idx * 90 + 40) % 255) as u8,
                    ((idx * 120 + 80) % 255) as u8,
                    255,
                ]);
                let img = RgbaImage::from_pixel(width, height, color);
                let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(delay_ms, 1));
                assert!(encoder.encode_frame(frame).is_ok());
            }
        }
        buf
    }

    fn make_test_png(width: u32, height: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(width, height, Rgba([40, 200, 255, 255]));
        let mut buf = Vec::new();
        assert!(
            DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
                .is_ok()
        );
        buf
    }

    #[test]
    fn extreme_aspect_ratios_clamp_to_at_least_one_cell() {
        // Panoramic 500x1 image (discordo bug: bounds.Dy() truncated to 0!).
        let wide = compute_image_cell_box(500, 1, 40, 16, (8, 16), MediaFitMode::Contain);
        assert!(wide.width >= 1 && wide.width <= 40);
        assert_eq!(wide.height, 1, "500x1 panoramic image must never produce 0 height");

        // Skyscraper 1x500 image.
        let tall = compute_image_cell_box(1, 500, 40, 16, (8, 16), MediaFitMode::Contain);
        assert_eq!(tall.width, 1, "1x500 tall image must never produce 0 width");
        assert!(tall.height >= 1 && tall.height <= 16);

        // Ultra-extreme 10_000x1 and 1x10_000 in tiny 16x2 area.
        let ultra_w = compute_image_cell_box(10_000, 1, 16, 2, (8, 16), MediaFitMode::Contain);
        assert_eq!(ultra_w.height, 1);
        assert!(ultra_w.width >= 1 && ultra_w.width <= 16);

        let ultra_h = compute_image_cell_box(1, 10_000, 16, 2, (8, 16), MediaFitMode::Contain);
        assert_eq!(ultra_h.width, 1);
        assert!(ultra_h.height >= 1 && ultra_h.height <= 2);

        // Zero bounds or zero source dimensions never divide by zero or panic.
        assert_eq!(
            compute_image_cell_box(500, 1, 0, 10, (8, 16), MediaFitMode::Contain),
            Size::new(0, 0)
        );
        assert_eq!(
            compute_image_cell_box(0, 0, 40, 16, (0, 0), MediaFitMode::Contain),
            Size::new(1, 1)
        );

        // Pixel clamping also never returns 0.
        assert_eq!(aspect_clamped_pixels(500, 1, 320, 320), (320, 1));
        assert_eq!(aspect_clamped_pixels(1, 500, 320, 320), (1, 320));
    }

    #[test]
    fn gif_multi_frame_decode_and_frame_limits() {
        let gif_bytes = make_test_gif(5, 24, 16, 60);
        let decoded = decode_image_bytes(
            "gif://dancing-cat",
            &gif_bytes,
            Some("dancing cat".to_owned()),
            &DecodeLimits::default(),
        );
        let media = match decoded {
            Ok(m) => m,
            Err(err) => panic!("expected valid multi-frame GIF decode, got {err}"),
        };

        assert!(media.is_animated);
        assert_eq!(media.frame_count(), 5);
        assert_eq!(media.source_width, 24);
        assert_eq!(media.source_height, 16);
        for frame in &media.frames {
            assert_eq!(frame.width, 24);
            assert_eq!(frame.height, 16);
            assert_eq!(frame.pixels.len(), 24 * 16 * 4);
            assert_eq!(frame.delay_ms, 60);
        }

        // Respects max_animated_frames ceiling.
        let tight = DecodeLimits {
            max_animated_frames: 2,
            ..DecodeLimits::default()
        };
        let limited = decode_image_bytes("gif://capped", &gif_bytes, None, &tight);
        assert_eq!(limited.map(|m| m.frame_count()).ok(), Some(2));

        // Corrupt bytes return clean error with zero panics.
        assert!(decode_image_bytes("bad", b"GIF89a\x00corrupt", None, &DecodeLimits::default()).is_err());
        assert!(decode_image_bytes("empty", b"", None, &DecodeLimits::default()).is_err());
    }

    #[test]
    fn byte_bounded_lru_cache_evicts_oldest_strictly_within_budget() {
        let make_item = |key: &str, byte_len: usize| {
            let pixels = vec![255u8; byte_len];
            let frame = RgbaFrame {
                width: (byte_len / 4).max(1) as u32,
                height: 1,
                delay_ms: 0,
                pixels,
            };
            Arc::new(DecodedMedia::from_rgba_frames(key, None, 10, 1, vec![frame]))
        };

        let mut cache = MediaFrameCache::new(100);
        let out1 = cache.insert(make_item("a", 40));
        assert!(out1.cached);
        assert!(out1.evicted_keys.is_empty());

        let out2 = cache.insert(make_item("b", 40));
        assert!(out2.cached);
        assert_eq!(cache.total_bytes(), 80);

        // Touch "a" so "b" becomes least recently used.
        assert!(cache.get("a").is_some());

        // Inserting "c" (40 bytes) exceeds 100 -> must evict "b", keeping "a" + "c" (80 bytes).
        let out3 = cache.insert(make_item("c", 40));
        assert!(out3.cached);
        assert_eq!(out3.evicted_keys, vec!["b".to_owned()]);
        assert_eq!(cache.total_bytes(), 80);
        assert!(cache.peek("a").is_some());
        assert!(cache.peek("b").is_none());
        assert!(cache.peek("c").is_some());

        // Oversized single item (> budget) is cleanly rejected without corrupting budget.
        let oversized = cache.insert(make_item("huge", 200));
        assert!(!oversized.cached);
        assert!(cache.total_bytes() <= cache.budget_bytes());
    }

    #[tokio::test]
    async fn background_decoder_pool_caches_static_and_animated_media() {
        let pool = MediaDecoderPool::new(2, 1024 * 1024, DecodeLimits::default());
        let png = make_test_png(32, 24);

        let first = pool
            .decode_and_cache("img://hero".to_owned(), png.clone(), Some("hero.png".to_owned()))
            .await;
        let (media, evicted) = match first {
            Ok(v) => v,
            Err(err) => panic!("png decode failed: {err}"),
        };
        assert!(evicted.is_empty());
        assert_eq!(media.frame_count(), 1);
        assert!(pool.cache_bytes() >= 32 * 24 * 4);

        // Second lookup returns the cached Arc immediately even if empty bytes passed (cache hit).
        let second = pool
            .decode_and_cache("img://hero".to_owned(), Vec::new(), None)
            .await;
        let (cached_media, _) = match second {
            Ok(v) => v,
            Err(err) => panic!("cache hit failed: {err}"),
        };
        assert!(Arc::ptr_eq(&media, &cached_media));
    }

    #[test]
    fn multi_tier_renderer_renders_all_six_tiers_into_test_backend() {
        let gif_bytes = make_test_gif(2, 16, 16, 80);
        let gif_media = match decode_image_bytes(
            "gif://wave",
            &gif_bytes,
            Some("wave".to_owned()),
            &DecodeLimits::default(),
        ) {
            Ok(m) => m,
            Err(err) => panic!("decode failed: {err}"),
        };

        let tiers = [
            GraphicsTier::KittyPlaceholder,
            GraphicsTier::KittyDirect,
            GraphicsTier::Sixel,
            GraphicsTier::ITerm2,
            GraphicsTier::HalfBlock,
            GraphicsTier::TextTagOnly,
        ];

        for tier in tiers {
            let renderer = MediaRenderer::new(tier, ResolvedCharset::Utf8, (8, 16), false);
            let rendered = match renderer.render_frame(&gif_media, 0, 20, 8, MediaFitMode::Contain)
            {
                Ok(r) => r,
                Err(err) => panic!("render failed for {tier:?}: {err}"),
            };
            assert_eq!(rendered.tier(), tier);
            assert!(rendered.size().width >= 1);
            assert!(rendered.size().height >= 1);

            let backend = TestBackend::new(30, 12);
            let mut terminal = match Terminal::new(backend) {
                Ok(t) => t,
                Err(err) => panic!("TestBackend init failed: {err}"),
            };
            assert!(
                terminal
                    .draw(|f| {
                        f.render_widget(MediaWidget::new(&rendered), Rect::new(0, 0, 20, 8));
                    })
                    .is_ok()
            );

            if tier == GraphicsTier::TextTagOnly {
                let row0: String = (0..11)
                    .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
                    .collect();
                assert_eq!(row0, "[GIF: wave]");
            }
        }
    }

    #[test]
    fn text_tag_sanitizes_ascii7bit_for_hd44780_lcd() {
        let frame = RgbaFrame {
            width: 1920,
            height: 1080,
            delay_ms: 0,
            pixels: vec![0; 4],
        };
        let media = DecodedMedia::from_rgba_frames(
            "img://1",
            Some("café\\pic~".to_owned()),
            1920,
            1080,
            vec![frame],
        );
        let tag = TextTagFrame::new(&media, ResolvedCharset::Ascii7Bit, 16);
        // Should fit within 16 cells and contain strictly valid 7-bit HD44780 bytes (no '\' or '~').
        assert!(tag.label.len() <= 16);
        assert!(
            tag.label
                .bytes()
                .all(|b| (0x20..=0x7D).contains(&b) && b != b'\\')
        );
        assert_eq!(tag.label, "[IMG: 1920x1080]");
    }

    #[test]
    fn pty_bandwidth_governor_enforces_local_and_ssh_budgets() {
        let t0 = Instant::now();
        let local_gov = PtyBandwidthGovernor::for_session(false);
        assert_eq!(local_gov.budget_bytes_per_sec(), 2 * 1024 * 1024);

        let mut ssh_gov = PtyBandwidthGovernor::for_session(true);
        assert_eq!(ssh_gov.budget_bytes_per_sec(), 128 * 1024);
        assert!(ssh_gov.can_emit(80_000, t0));
        ssh_gov.record_emitted(80_000, t0);
        assert!(!ssh_gov.can_emit(80_000, t0 + Duration::from_millis(200)));
        // After the 1-second window expires, budget resets cleanly.
        assert!(ssh_gov.can_emit(80_000, t0 + Duration::from_millis(1050)));
    }

    #[test]
    fn pty_bandwidth_governor_allows_oversized_static_when_idle_and_throttles_animations() {
        let t0 = Instant::now();
        let mut ssh_gov = PtyBandwidthGovernor::for_session(true);
        let oversized = SSH_PTY_BYTE_BUDGET_PER_SEC * 2 + 4096;

        // When window_bytes == 0, an oversized single static image MUST be allowed (token-bucket debt).
        assert!(ssh_gov.can_emit_static(oversized, t0));
        assert!(ssh_gov.can_emit_for(oversized, false, t0));
        // Animations NEVER incur token-bucket debt above their sub-budget.
        assert!(!ssh_gov.can_emit_animation(oversized, t0));
        assert!(!ssh_gov.can_emit_for(oversized, true, t0));

        // Record the oversized static emission -> subsequent animations and static frames throttle.
        ssh_gov.record_emitted(oversized, t0);
        let t_mid = t0 + Duration::from_millis(250);
        assert!(!ssh_gov.can_emit_animation(1024, t_mid));
        assert!(!ssh_gov.can_emit_static(1024, t_mid));

        // After the 1-second window clears, static images can occupy up to 100% while animations
        // throttle first at 75% load.
        let t_next = t0 + Duration::from_millis(1050);
        let eighty_pct = (SSH_PTY_BYTE_BUDGET_PER_SEC * 4) / 5;
        assert!(ssh_gov.can_emit_static(eighty_pct, t_next));
        ssh_gov.record_emitted(eighty_pct, t_next);
        assert!(!ssh_gov.can_emit_animation(512, t_next + Duration::from_millis(50)));
        assert!(ssh_gov.can_emit_static(8 * 1024, t_next + Duration::from_millis(50)));
    }

    #[test]
    fn static_jpeg_and_webp_decode_and_extreme_aspect_render_never_panics() {
        // JPEG encode -> decode round-trip.
        let rgb_img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            20,
            12,
            image::Rgb([180, 90, 240]),
        ));
        let mut jpeg_bytes = Vec::new();
        assert!(
            rgb_img
                .write_to(&mut Cursor::new(&mut jpeg_bytes), ImageFormat::Jpeg)
                .is_ok()
        );
        let jpeg_decoded = decode_image_bytes(
            "img://jpeg",
            &jpeg_bytes,
            Some("photo.jpg".to_owned()),
            &DecodeLimits::default(),
        );
        assert_eq!(
            jpeg_decoded.map(|m| (m.source_width, m.source_height, m.is_animated)).ok(),
            Some((20, 12, false))
        );

        // WebP encode -> decode round-trip.
        let rgba_img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            18,
            14,
            Rgba([20, 220, 120, 255]),
        ));
        let mut webp_bytes = Vec::new();
        assert!(
            rgba_img
                .write_to(&mut Cursor::new(&mut webp_bytes), ImageFormat::WebP)
                .is_ok()
        );
        let webp_decoded = decode_image_bytes(
            "img://webp",
            &webp_bytes,
            Some("sticker.webp".to_owned()),
            &DecodeLimits::default(),
        );
        assert_eq!(
            webp_decoded.map(|m| (m.source_width, m.source_height, m.is_animated)).ok(),
            Some((18, 14, false))
        );

        // Render extreme 500x1 and 1x500 decoded media through HalfBlock & KittyDirect without panic.
        let wide_png = make_test_png(500, 1);
        let tall_png = make_test_png(1, 500);
        let wide_media = match decode_image_bytes("img://wide", &wide_png, None, &DecodeLimits::default()) {
            Ok(m) => m,
            Err(err) => panic!("wide decode failed: {err}"),
        };
        let tall_media = match decode_image_bytes("img://tall", &tall_png, None, &DecodeLimits::default()) {
            Ok(m) => m,
            Err(err) => panic!("tall decode failed: {err}"),
        };

        for tier in [
            GraphicsTier::HalfBlock,
            GraphicsTier::KittyDirect,
            GraphicsTier::KittyPlaceholder,
        ] {
            let renderer = MediaRenderer::new(tier, ResolvedCharset::Utf8, (8, 16), false);
            let rw = match renderer.render_frame(&wide_media, 0, 40, 16, MediaFitMode::Contain) {
                Ok(r) => r,
                Err(err) => panic!("wide render failed on {tier:?}: {err}"),
            };
            assert!(rw.size().width >= 1 && rw.size().height >= 1);

            let rt = match renderer.render_frame(&tall_media, 0, 40, 16, MediaFitMode::Contain) {
                Ok(r) => r,
                Err(err) => panic!("tall render failed on {tier:?}: {err}"),
            };
            assert!(rt.size().width >= 1 && rt.size().height >= 1);
        }
    }

    fn make_test_animated_webp(
        frame_count: usize,
        width: u32,
        height: u32,
        delay_ms: u32,
    ) -> Vec<u8> {
        let mut body = Vec::new();

        // VP8X extended header chunk (10 bytes): bit 1 = Animation (0x02), bit 4 = Alpha (0x10).
        let mut vp8x = [0u8; 10];
        vp8x[0] = 0x12;
        let w_m1 = width - 1;
        let h_m1 = height - 1;
        vp8x[4] = (w_m1 & 0xFF) as u8;
        vp8x[5] = ((w_m1 >> 8) & 0xFF) as u8;
        vp8x[6] = ((w_m1 >> 16) & 0xFF) as u8;
        vp8x[7] = (h_m1 & 0xFF) as u8;
        vp8x[8] = ((h_m1 >> 8) & 0xFF) as u8;
        vp8x[9] = ((h_m1 >> 16) & 0xFF) as u8;
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(&vp8x);

        // ANIM chunk (6 bytes): BGRA background = transparent black, loop count = 0 (infinite).
        body.extend_from_slice(b"ANIM");
        body.extend_from_slice(&6u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 6]);

        for idx in 0..frame_count {
            let color = Rgba([
                ((idx * 60 + 20) % 255) as u8,
                ((idx * 80 + 40) % 255) as u8,
                ((idx * 100 + 60) % 255) as u8,
                255,
            ]);
            let img = RgbaImage::from_pixel(width, height, color);
            let mut single = Vec::new();
            assert!(
                DynamicImage::ImageRgba8(img)
                    .write_to(&mut Cursor::new(&mut single), ImageFormat::WebP)
                    .is_ok()
            );
            // Strip the 12-byte `RIFF <size> WEBP` container header to extract the VP8L chunk.
            let frame_chunk = &single[12..];

            let mut anmf = Vec::with_capacity(16 + frame_chunk.len());
            // Frame X/Y offsets (0, 0) in 24-bit LE.
            anmf.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            // Frame width-1 & height-1 in 24-bit LE.
            anmf.extend_from_slice(&[
                (w_m1 & 0xFF) as u8,
                ((w_m1 >> 8) & 0xFF) as u8,
                ((w_m1 >> 16) & 0xFF) as u8,
                (h_m1 & 0xFF) as u8,
                ((h_m1 >> 8) & 0xFF) as u8,
                ((h_m1 >> 16) & 0xFF) as u8,
            ]);
            // Frame duration in ms (24-bit LE) + flags (0x02 = do not blend).
            anmf.extend_from_slice(&[
                (delay_ms & 0xFF) as u8,
                ((delay_ms >> 8) & 0xFF) as u8,
                ((delay_ms >> 16) & 0xFF) as u8,
                0x02,
            ]);
            anmf.extend_from_slice(frame_chunk);

            body.extend_from_slice(b"ANMF");
            body.extend_from_slice(&(anmf.len() as u32).to_le_bytes());
            body.extend_from_slice(&anmf);
            if anmf.len() % 2 == 1 {
                body.push(0);
            }
        }

        let mut riff = Vec::with_capacity(12 + body.len());
        riff.extend_from_slice(b"RIFF");
        riff.extend_from_slice(&((4 + body.len()) as u32).to_le_bytes());
        riff.extend_from_slice(b"WEBP");
        riff.extend_from_slice(&body);
        riff
    }

    #[test]
    fn animated_webp_multi_frame_decode_and_limits() {
        assert_eq!(DEFAULT_MAX_STATIC_DIM, 1024);
        assert_eq!(DEFAULT_DECODE_CONCURRENCY, 8);

        let webp_bytes = make_test_animated_webp(4, 24, 16, 75);
        let decoded = decode_image_bytes(
            "webp://klipy-meme",
            &webp_bytes,
            Some("meme.webp".to_owned()),
            &DecodeLimits::default(),
        );
        let media = match decoded {
            Ok(m) => m,
            Err(err) => panic!("expected animated WebP multi-frame decode, got {err}"),
        };

        assert!(media.is_animated);
        assert_eq!(media.frame_count(), 4);
        assert_eq!(media.source_width, 24);
        assert_eq!(media.source_height, 16);
        for (idx, frame) in media.frames.iter().enumerate() {
            assert_eq!(frame.width, 24);
            assert_eq!(frame.height, 16);
            assert_eq!(frame.delay_ms, 75);
            assert_eq!(frame.pixels.len(), 24 * 16 * 4);
            assert_eq!(frame.pixels[0], ((idx * 60 + 20) % 255) as u8);
            assert_eq!(frame.pixels[1], ((idx * 80 + 40) % 255) as u8);
            assert_eq!(frame.pixels[2], ((idx * 100 + 60) % 255) as u8);
        }

        // Respects max_animated_frames ceiling.
        let tight_frames = DecodeLimits {
            max_animated_frames: 2,
            ..DecodeLimits::default()
        };
        let limited = decode_image_bytes("webp://capped", &webp_bytes, None, &tight_frames);
        assert_eq!(limited.map(|m| m.frame_count()).ok(), Some(2));

        // Respects max_animated_dimension downscaling while preserving intrinsic source dimensions.
        let large_webp = make_test_animated_webp(2, 400, 200, 90);
        let scaled = match decode_image_bytes(
            "webp://large",
            &large_webp,
            None,
            &DecodeLimits::default(),
        ) {
            Ok(m) => m,
            Err(err) => panic!("expected downscaled animated WebP decode, got {err}"),
        };
        assert!(scaled.is_animated);
        assert_eq!((scaled.source_width, scaled.source_height), (400, 200));
        assert_eq!((scaled.frames[0].width, scaled.frames[0].height), (320, 160));
    }

    #[test]
    fn kitty_direct_chunks_large_images_and_doubles_escapes_in_tmux_passthrough() {
        use std::io::Read as _;

        // Pseudo-noise 96x96 image ensures compressed zlib stream spans multiple 4096-byte chunks.
        let mut noisy = RgbaImage::new(96, 96);
        let mut state: u32 = 0x1234_5678;
        for px in noisy.pixels_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = (state >> 24) as u8;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let g = (state >> 24) as u8;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let b = (state >> 24) as u8;
            *px = Rgba([r, g, b, 255]);
        }
        let img = DynamicImage::ImageRgba8(noisy.clone());
        let direct = KittyDirectFrame::encode(&img, Size::new(12, 6), 123, false);
        assert!(
            direct
                .payload
                .starts_with("\x1b_Ga=T,f=32,o=z,s=96,v=96,c=12,r=6,i=123,C=1,q=2,m=1;")
        );
        assert!(direct.payload.contains("\x1b_Gm=1;"));
        assert!(direct.payload.contains("\x1b_Gm=0;"));
        assert!(direct.payload.ends_with("\x1b\\"));

        // Reassemble base64 chunks and verify zlib inflate restores exact raw RGBA bytes.
        let mut reassembled_b64 = String::new();
        for part in direct.payload.split("\x1b_G").skip(1) {
            let trimmed = part.strip_suffix("\x1b\\").unwrap_or_default();
            let (_, data) = trimmed.split_once(';').unwrap_or_default();
            reassembled_b64.push_str(data);
        }
        let compressed = base64::engine::general_purpose::STANDARD
            .decode(reassembled_b64.as_bytes())
            .unwrap_or_default();
        let mut inflated = Vec::new();
        let mut decoder = flate2::read::ZlibDecoder::new(compressed.as_slice());
        assert!(decoder.read_to_end(&mut inflated).is_ok());
        assert_eq!(inflated, *noisy.as_raw());

        // Tmux DCS passthrough MUST wrap each chunk with `\x1bPtmux;\x1b\x1b_G...\x1b\x1b\\\x1b\\`
        // and include `o=z` and `C=1` on chunk 0 so terminal cursor tracking stays synced.
        let small = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])));
        let tmux_direct = KittyDirectFrame::encode(&small, Size::new(1, 1), 77, true);
        assert!(
            tmux_direct
                .payload
                .starts_with("\x1bPtmux;\x1b\x1b_Ga=T,f=32,o=z,s=2,v=2,c=1,r=1,i=77,C=1,q=2,m=0;")
        );
        assert!(tmux_direct.payload.ends_with("\x1b\x1b\\\x1b\\"));
    }

    #[test]
    fn kitty_placeholder_and_direct_emit_oz_compressed_payloads() {
        // Flat banded 128x64 graphic compresses dramatically under fast zlib (`o=z`).
        let mut img = RgbaImage::new(128, 64);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            *px = Rgba([((x / 16) * 32) as u8, 96, 180, 255]);
        }
        let raw_b64_len = (128 * 64 * 4) * 4 / 3;
        let frame = RgbaFrame {
            width: 128,
            height: 64,
            delay_ms: 0,
            pixels: img.clone().into_raw(),
        };
        let media = DecodedMedia::from_rgba_frames("img://bands", None, 128, 64, vec![frame]);

        // KittyDirect: verify `o=z` header and >= 10x shrink on banded graphic.
        let direct = KittyDirectFrame::encode(&DynamicImage::ImageRgba8(img), Size::new(16, 4), 200, false);
        assert!(direct.payload.contains("f=32,o=z,"));
        assert!(
            direct.payload.len() < raw_b64_len / 4,
            "expected compressed KittyDirect payload ({}) << uncompressed ({})",
            direct.payload.len(),
            raw_b64_len
        );

        // KittyPlaceholder: render first frame into a buffer and verify `o=z` in emitted sequence.
        let renderer = MediaRenderer::new(GraphicsTier::KittyPlaceholder, ResolvedCharset::Utf8, (8, 16), false);
        let rendered = match renderer.render_frame(&media, 0, 16, 4, MediaFitMode::Contain) {
            Ok(r) => r,
            Err(err) => panic!("KittyPlaceholder render failed: {err}"),
        };
        let mut buf = Buffer::empty(Rect::new(0, 0, 16, 4));
        rendered.render(Rect::new(0, 0, 16, 4), &mut buf);
        let first_cell = buf[(0, 0)].symbol();
        assert!(
            first_cell.contains("f=32,o=z,"),
            "expected KittyPlaceholder transmission to carry o=z compression"
        );
        assert!(
            first_cell.len() < raw_b64_len / 4,
            "expected compressed KittyPlaceholder payload ({}) << uncompressed ({})",
            first_cell.len(),
            raw_b64_len
        );
    }

    #[tokio::test]
    async fn media_encoder_pool_encodes_off_thread_and_deduplicates_in_flight() {
        let frame = Arc::new(RgbaFrame {
            width: 32,
            height: 32,
            delay_ms: 0,
            pixels: vec![128u8; 32 * 32 * 4],
        });

        let (pool, mut rx) = MediaEncoderPool::new(2);
        assert!(pool.enqueue(
            "img://kitty",
            Arc::clone(&frame),
            8,
            4,
            GraphicsTier::KittyPlaceholder,
            (8, 16),
        ));
        // Duplicate `(key, w, h, tier)` submitted while still queued/in-flight is deduplicated.
        assert!(!pool.enqueue(
            "img://kitty",
            Arc::clone(&frame),
            8,
            4,
            GraphicsTier::KittyPlaceholder,
            (8, 16),
        ));
        assert!(pool.is_in_flight("img://kitty", 8, 4, GraphicsTier::KittyPlaceholder));
        assert!(pool.enqueue(
            "img://direct",
            Arc::clone(&frame),
            8,
            4,
            GraphicsTier::KittyDirect,
            (8, 16),
        ));
        assert!(pool.enqueue(
            "img://sixel",
            Arc::clone(&frame),
            8,
            4,
            GraphicsTier::Sixel,
            (8, 16),
        ));
        assert!(pool.enqueue(
            "img://halfblock",
            Arc::clone(&frame),
            8,
            4,
            GraphicsTier::HalfBlock,
            (8, 16),
        ));

        let mut received = std::collections::HashMap::new();
        for _ in 0..4 {
            let item = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("encoder pool timed out")
                .expect("encoder pool channel closed prematurely");
            let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
            item.render(Rect::new(0, 0, 8, 4), &mut buf);
            if item.tier == GraphicsTier::KittyPlaceholder {
                assert!(buf[(0, 0)].symbol().contains("o=z"));
            }
            if let RenderedMedia::KittyDirect(ref kd) = item.media {
                assert!(kd.payload.contains("o=z"));
            }
            received.insert(item.key.clone(), item.tier);
        }

        assert_eq!(received.get("img://kitty"), Some(&GraphicsTier::KittyPlaceholder));
        assert_eq!(received.get("img://direct"), Some(&GraphicsTier::KittyDirect));
        assert_eq!(received.get("img://sixel"), Some(&GraphicsTier::Sixel));
        assert_eq!(received.get("img://halfblock"), Some(&GraphicsTier::HalfBlock));

        // After completion, in-flight entries are cleaned up via RAII guard.
        assert!(!pool.is_in_flight("img://kitty", 8, 4, GraphicsTier::KittyPlaceholder));

        // Direct async `encode_once` also returns a ready-to-draw frame cleanly.
        let req = MediaEncodeRequest::new(
            "img://once",
            frame,
            6,
            3,
            GraphicsTier::KittyDirect,
            (8, 16),
        );
        let once = pool.encode_once(req).await;
        assert!(once.is_ok());
        assert_eq!(once.map(|f| f.tier).ok(), Some(GraphicsTier::KittyDirect));
    }

    #[test]
    fn renderer_builds_stateful_protocol_and_halfblock_proxy() {
        let png = make_test_png(32, 32);
        let media = match decode_image_bytes("img://sq", &png, None, &DecodeLimits::default()) {
            Ok(m) => m,
            Err(err) => panic!("decode failed: {err}"),
        };
        let renderer = MediaRenderer::new(GraphicsTier::Sixel, ResolvedCharset::Utf8, (9, 18), false);
        assert_eq!(renderer.picker().protocol_type(), ProtocolType::Sixel);
        assert!(renderer.create_stateful_protocol(&media.frames[0]).is_ok());

        let proxy = match renderer.render_halfblock_proxy(&media, 0, 12, 6, MediaFitMode::Contain) {
            Ok(p) => p,
            Err(err) => panic!("proxy failed: {err}"),
        };
        assert_eq!(proxy.tier(), GraphicsTier::HalfBlock);

        // KittyPlaceholder uses StatefulKitty (`U=1`), whereas KittyDirect MUST NOT route to
        // StatefulKitty (unsupported by WezTerm/Konsole) and instead renders via KittyDirectFrame.
        let kp_renderer =
            MediaRenderer::new(GraphicsTier::KittyPlaceholder, ResolvedCharset::Utf8, (8, 16), false);
        assert_eq!(kp_renderer.picker().protocol_type(), ProtocolType::Kitty);
        let kp_stateful = match kp_renderer.create_stateful_protocol(&media.frames[0]) {
            Ok(s) => s,
            Err(err) => panic!("KittyPlaceholder stateful protocol failed: {err}"),
        };
        assert!(matches!(
            kp_stateful.protocol_type(),
            StatefulProtocolType::Kitty(_)
        ));

        let kd_renderer =
            MediaRenderer::new(GraphicsTier::KittyDirect, ResolvedCharset::Utf8, (8, 16), false);
        assert_eq!(kd_renderer.picker().protocol_type(), ProtocolType::Halfblocks);
        assert!(kd_renderer.create_stateful_protocol(&media.frames[0]).is_err());
        let kd_frame = match kd_renderer.render_frame(&media, 0, 12, 6, MediaFitMode::Contain) {
            Ok(f) => f,
            Err(err) => panic!("KittyDirect render_frame failed: {err}"),
        };
        assert!(matches!(kd_frame, RenderedMedia::KittyDirect(_)));
    }
}
