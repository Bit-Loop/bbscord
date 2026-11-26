//! btop-style meters: Braille sparklines, VU bars, progress bars and voice-message waveforms (§7).
//!
//! Every function returns **exactly `width` cells**, each a single-column glyph, so callers can
//! lay them out without measuring. Inputs are clamped: NaN → 0, ±inf → the nearest bound.
//!
//! Glyph tiers follow §1.3: `Utf8` gets Braille/eighth blocks, `VtSafe` only glyphs present in
//! Linux console PSF fonts (`▄ ▌ █ ░`), `Ascii7Bit` only `0x20..=0x7D` minus `\`.

use bbs_core::ResolvedCharset;

/// Braille bit for each filled row of the left column, bottom-up (§7).
pub const BRAILLE_LEFT_BITS: [u32; 4] = [6, 2, 1, 0];
/// Braille bit for each filled row of the right column, bottom-up (§7).
pub const BRAILLE_RIGHT_BITS: [u32; 4] = [7, 5, 4, 3];

/// dBFS floor of the VU scale (§7: `−60..0 dBFS`).
pub const VU_FLOOR_DB: f32 = -60.0;
/// VU gradient mid stop (§7: −18 dBFS).
pub const VU_MID_DB: f32 = -18.0;
/// VU gradient end stop (§7: −6 dBFS).
pub const VU_HOT_DB: f32 = -6.0;

/// Clamp to `[0,1]`, mapping NaN to 0.
#[must_use]
pub fn norm(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }
}

/// `round(norm(v) * steps)` as an integer in `0..=steps`.
fn level(v: f32, steps: usize) -> usize {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let l = (norm(v) * steps as f32).round() as usize;
    l.min(steps)
}

/// Braille cell for a left/right column fill height (each `0..=4`), bottom-up (§7).
#[must_use]
pub fn braille_column_bits(left_h: u8, right_h: u8) -> char {
    let mut bits = 0u32;
    for b in BRAILLE_LEFT_BITS.iter().take(usize::from(left_h.min(4))) {
        bits |= 1 << b;
    }
    for b in BRAILLE_RIGHT_BITS.iter().take(usize::from(right_h.min(4))) {
        bits |= 1 << b;
    }
    char::from_u32(0x2800 | bits).unwrap_or(' ')
}

/// Right-aligned window of the newest `n` samples, left-padded with `None`.
fn window(samples: &[f32], n: usize) -> impl Iterator<Item = Option<f32>> + '_ {
    let pad = n.saturating_sub(samples.len());
    let start = samples.len().saturating_sub(n);
    std::iter::repeat_n(None, pad).chain(samples[start..].iter().copied().map(Some))
}

/// Sparkline of normalized samples (`0..=1`), exactly `width` cells, newest at the right.
///
/// - `Utf8`: Braille, **2 samples per cell** (left/right dot columns, 4 rows each), so 8 cells
///   show 16 samples (§7).
/// - `VtSafe`: one sample per cell: ` ` / `▄` / `█`.
/// - `Ascii7Bit`: one sample per cell from the levels ` ._-=#`.
#[must_use]
pub fn sparkline(samples: &[f32], width: usize, charset: ResolvedCharset) -> String {
    match charset {
        ResolvedCharset::Utf8 => {
            let h = |s: Option<f32>| -> u8 {
                // level(.., 4) ≤ 4, fits u8.
                #[allow(clippy::cast_possible_truncation)]
                let l = s.map_or(0, |v| level(v, 4)) as u8;
                l
            };
            let w: Vec<Option<f32>> = window(samples, width * 2).collect();
            w.as_chunks::<2>()
                .0
                .iter()
                .map(|p| braille_column_bits(h(p[0]), h(p[1])))
                .collect()
        }
        ResolvedCharset::VtSafe => window(samples, width)
            .map(|s| [' ', '▄', '█'][s.map_or(0, |v| level(v, 2))])
            .collect(),
        ResolvedCharset::Ascii7Bit => window(samples, width)
            .map(|s| [' ', '.', '_', '-', '=', '#'][s.map_or(0, |v| level(v, 5))])
            .collect(),
    }
}

/// Single-sample-per-cell sparkline using eighth blocks `▁..█` in `Utf8` (blank for 0);
/// identical to [`sparkline`] in the lower tiers. Exactly `width` cells, newest at the right.
#[must_use]
pub fn sparkline_eighths(samples: &[f32], width: usize, charset: ResolvedCharset) -> String {
    const E: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    match charset {
        ResolvedCharset::Utf8 => window(samples, width)
            .map(|s| E[s.map_or(0, |v| level(v, 8))])
            .collect(),
        _ => sparkline(samples, width, charset),
    }
}

/// Map a dBFS reading onto the VU's `0..=1` scale (`−60 dB → 0`, `0 dB → 1`; NaN → 0).
#[must_use]
pub fn dbfs_to_level(db: f32) -> f32 {
    if db.is_nan() {
        return 0.0;
    }
    norm((db - VU_FLOOR_DB) / -VU_FLOOR_DB)
}

/// Gradient position for a VU cell at scale position `level` (`0..=1`), with the §7 stops:
/// start→mid over `−60..−18 dB`, mid→end over `−18..−6 dB`, end above `−6 dB`.
#[must_use]
pub fn vu_gradient_t(level: f32) -> f32 {
    let mid = dbfs_to_level(VU_MID_DB);
    let hot = dbfs_to_level(VU_HOT_DB);
    let l = norm(level);
    if l <= mid {
        0.5 * l / mid
    } else if l <= hot {
        0.5 + 0.5 * (l - mid) / (hot - mid)
    } else {
        1.0
    }
}

/// Number of cells of a `width`-cell VU that contain any fill for `level`
/// (cells at index `>= vu_lit_cells` are the empty track, coloured `Role::MeterBg`).
#[must_use]
pub fn vu_lit_cells(level: f32, width: usize, charset: ResolvedCharset) -> usize {
    match charset {
        ResolvedCharset::Ascii7Bit => self::level(level, width),
        _ => self::level(level, width * 2).div_ceil(2),
    }
}

/// Horizontal VU meter cells for `level` (`0..=1`), exactly `width` chars.
///
/// - `Utf8`: `2·width` half-columns; full `⣿`, half `⡇`, empty track `⣀`.
/// - `VtSafe`: full `█`, half `▌`, empty track `░`.
/// - `Ascii7Bit`: `#` / `-` (wrap in `[…]` yourself for the `[####----]` look).
#[must_use]
pub fn vu_cells(level: f32, width: usize, charset: ResolvedCharset) -> Vec<char> {
    let (full, half, off) = match charset {
        ResolvedCharset::Utf8 => ('⣿', '⡇', '⣀'),
        ResolvedCharset::VtSafe => ('█', '▌', '░'),
        ResolvedCharset::Ascii7Bit => {
            let n = self::level(level, width);
            return (0..width).map(|i| if i < n { '#' } else { '-' }).collect();
        }
    };
    let halves = self::level(level, width * 2);
    (0..width)
        .map(|i| {
            if halves >= 2 * i + 2 {
                full
            } else if halves == 2 * i + 1 {
                half
            } else {
                off
            }
        })
        .collect()
}

/// Progress/poll bar for `frac` (`0..=1`), exactly `width` cells.
///
/// - `Utf8`: eighth-precision `▏▎▍▌▋▊▉█`, empty `░`.
/// - `VtSafe`: `█` / `▌`, empty `░`.
/// - `Ascii7Bit`: `#` / `-`.
#[must_use]
pub fn bar(frac: f32, width: usize, charset: ResolvedCharset) -> String {
    const PART: [char; 8] = ['░', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    match charset {
        ResolvedCharset::Utf8 => {
            let eighths = level(frac, width * 8);
            (0..width)
                .map(|i| {
                    let lo = i * 8;
                    if eighths >= lo + 8 {
                        '█'
                    } else if eighths > lo {
                        PART[eighths - lo]
                    } else {
                        '░'
                    }
                })
                .collect()
        }
        _ => vu_cells(frac, width, charset).into_iter().collect(),
    }
}

/// Voice-message waveform from Discord amplitude bytes (`0..=255`), exactly `width` cells.
///
/// Samples are resampled by taking the peak of each cell's bucket (or the nearest sample when
/// there are fewer samples than cells).
/// - `Utf8`: `▁▂▃▄▅▆▇█`
/// - `VtSafe`: `▄` (quiet half) / `█` (loud half)
/// - `Ascii7Bit`: ` .:|`
#[must_use]
pub fn waveform(samples: &[u8], width: usize, charset: ResolvedCharset) -> String {
    let n = samples.len();
    let peak = |i: usize| -> u8 {
        if n == 0 {
            return 0;
        }
        let lo = i * n / width;
        let hi = ((i + 1) * n / width).max(lo + 1).min(n);
        samples[lo.min(n - 1)..hi]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
    };
    (0..width)
        .map(|i| {
            let v = usize::from(peak(i));
            match charset {
                ResolvedCharset::Utf8 => ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][v / 32],
                ResolvedCharset::VtSafe => {
                    if v < 128 {
                        '▄'
                    } else {
                        '█'
                    }
                }
                ResolvedCharset::Ascii7Bit => [' ', '.', ':', '|'][v / 64],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthChar;

    const CHARSETS: [ResolvedCharset; 3] = [
        ResolvedCharset::Utf8,
        ResolvedCharset::VtSafe,
        ResolvedCharset::Ascii7Bit,
    ];

    fn cells(s: &str) -> usize {
        s.chars().map(|c| c.width().unwrap_or(0)).sum()
    }

    fn ascii_ok(s: &str) -> bool {
        s.bytes().all(|b| (0x20..=0x7D).contains(&b) && b != b'\\')
    }

    #[test]
    fn braille_bit_vectors() {
        assert_eq!(braille_column_bits(0, 0), '\u{2800}');
        assert_eq!(braille_column_bits(1, 0), '\u{2840}'); // bit 6
        assert_eq!(braille_column_bits(0, 1), '\u{2880}'); // bit 7
        assert_eq!(braille_column_bits(2, 0), '\u{2844}'); // 6,2
        assert_eq!(braille_column_bits(3, 0), '\u{2846}'); // 6,2,1
        assert_eq!(braille_column_bits(4, 0), '⡇'); // 6,2,1,0 = 0x47
        assert_eq!(braille_column_bits(0, 2), '\u{28A0}'); // 7,5
        assert_eq!(braille_column_bits(0, 3), '\u{28B0}'); // 7,5,4
        assert_eq!(braille_column_bits(0, 4), '\u{28B8}'); // 7,5,4,3
        assert_eq!(braille_column_bits(4, 4), '⣿');
        assert_eq!(braille_column_bits(9, 9), '⣿');
        assert_eq!(braille_column_bits(1, 1), '⣀');
    }

    #[test]
    fn width_invariants() {
        let samples: Vec<f32> = (0..37)
            .map(|i| match i % 7 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => f32::NEG_INFINITY,
                3 => -0.5,
                4 => 1.7,
                _ => i as f32 / 37.0,
            })
            .collect();
        let bytes: Vec<u8> = (0..=255).step_by(7).collect();
        for cs in CHARSETS {
            for w in 0..=40usize {
                for src in [&samples[..], &[][..], &samples[..3]] {
                    let s = sparkline(src, w, cs);
                    assert_eq!(s.chars().count(), w);
                    assert_eq!(cells(&s), w);
                    let e = sparkline_eighths(src, w, cs);
                    assert_eq!((e.chars().count(), cells(&e)), (w, w));
                }
                for lvl in [
                    f32::NAN,
                    f32::NEG_INFINITY,
                    -1.0,
                    0.0,
                    0.33,
                    0.5,
                    1.0,
                    2.0,
                    f32::INFINITY,
                ] {
                    let v = vu_cells(lvl, w, cs);
                    assert_eq!(v.len(), w);
                    assert!(v.iter().all(|c| c.width() == Some(1)));
                    assert!(vu_lit_cells(lvl, w, cs) <= w);
                    let b = bar(lvl, w, cs);
                    assert_eq!((b.chars().count(), cells(&b)), (w, w));
                    if cs == ResolvedCharset::Ascii7Bit {
                        assert!(ascii_ok(&b) && ascii_ok(&v.iter().collect::<String>()));
                    }
                }
                for src in [&bytes[..], &[][..], &bytes[..2]] {
                    let s = waveform(src, w, cs);
                    assert_eq!((s.chars().count(), cells(&s)), (w, w));
                    if cs == ResolvedCharset::Ascii7Bit {
                        assert!(ascii_ok(&s));
                    }
                }
                if cs == ResolvedCharset::Ascii7Bit {
                    assert!(ascii_ok(&sparkline(&samples, w, cs)));
                }
            }
        }
    }

    #[test]
    fn sparkline_two_samples_per_cell_newest_right() {
        let s = sparkline(&[0.0, 1.0, 1.0, 0.25], 2, ResolvedCharset::Utf8);
        let c: Vec<char> = s.chars().collect();
        assert_eq!(c[0], braille_column_bits(0, 4));
        assert_eq!(c[1], braille_column_bits(4, 1));
        // Fewer samples than slots: left-padded with blanks.
        assert_eq!(
            sparkline(&[1.0], 2, ResolvedCharset::Utf8),
            "\u{2800}\u{28B8}"
        );
        assert_eq!(
            sparkline(&[0.0, 0.5, 1.0], 3, ResolvedCharset::VtSafe),
            " ▄█"
        );
        assert_eq!(
            sparkline(
                &[0.0, 0.2, 0.4, 0.6, 0.8, 1.0],
                6,
                ResolvedCharset::Ascii7Bit
            ),
            " ._-=#"
        );
        assert_eq!(
            sparkline_eighths(&[0.0, 0.5, 1.0], 3, ResolvedCharset::Utf8),
            " ▄█"
        );
    }

    #[test]
    fn vu_glyphs() {
        let v: String = vu_cells(0.5, 4, ResolvedCharset::Utf8)
            .into_iter()
            .collect();
        assert_eq!(v, "⣿⣿⣀⣀");
        let v: String = vu_cells(0.625, 4, ResolvedCharset::Utf8)
            .into_iter()
            .collect();
        assert_eq!(v, "⣿⣿⡇⣀");
        assert_eq!(vu_lit_cells(0.625, 4, ResolvedCharset::Utf8), 3);
        let v: String = vu_cells(0.625, 4, ResolvedCharset::VtSafe)
            .into_iter()
            .collect();
        assert_eq!(v, "██▌░");
        let v: String = vu_cells(0.5, 8, ResolvedCharset::Ascii7Bit)
            .into_iter()
            .collect();
        assert_eq!(v, "####----");
        assert_eq!(dbfs_to_level(-60.0), 0.0);
        assert_eq!(dbfs_to_level(0.0), 1.0);
        assert_eq!(dbfs_to_level(-90.0), 0.0);
        assert_eq!(dbfs_to_level(f32::NAN), 0.0);
        assert!((vu_gradient_t(dbfs_to_level(-18.0)) - 0.5).abs() < 1e-5);
        assert!((vu_gradient_t(dbfs_to_level(-6.0)) - 1.0).abs() < 1e-5);
        assert_eq!(vu_gradient_t(0.0), 0.0);
        assert_eq!(vu_gradient_t(f32::NAN), 0.0);
    }

    #[test]
    fn bar_and_waveform_glyphs() {
        assert_eq!(bar(0.5, 4, ResolvedCharset::Utf8), "██░░");
        assert_eq!(bar(0.3125, 2, ResolvedCharset::Utf8), "▋░");
        assert_eq!(bar(1.0, 3, ResolvedCharset::Ascii7Bit), "###");
        assert_eq!(waveform(&[0, 255], 2, ResolvedCharset::Utf8), "▁█");
        assert_eq!(waveform(&[0, 255], 4, ResolvedCharset::Utf8), "▁▁██");
        assert_eq!(
            waveform(&[0, 100, 200, 255], 2, ResolvedCharset::Utf8),
            "▄█"
        );
        assert_eq!(waveform(&[10, 200], 2, ResolvedCharset::VtSafe), "▄█");
        assert_eq!(
            waveform(&[0, 64, 128, 255], 4, ResolvedCharset::Ascii7Bit),
            " .:|"
        );
        assert_eq!(waveform(&[], 3, ResolvedCharset::Utf8), "▁▁▁");
    }
}
