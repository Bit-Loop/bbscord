//! OKLab colour interpolation and 16-step gradient lookup tables (§7).
//!
//! Gradients are interpolated in OKLab (perceptually uniform: equal `t` steps look like equal
//! brightness/hue steps, and the midpoint of e.g. blue→yellow does not go muddy grey the way
//! naive sRGB lerp does), then quantized to [`GRADIENT_STEPS`] entries so that every border
//! run is a single SGR and a pane edge changes colour at most 16 times.

use bbs_core::{ColorGradient, RgbColor};

/// Number of quantized steps in every gradient lookup table (§7).
pub const GRADIENT_STEPS: usize = 16;

/// A colour in the OKLab space (`l` lightness, `a` green–red, `b` blue–yellow).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklab {
    /// Perceptual lightness (`0.0..=1.0`).
    pub l: f64,
    /// Green (−) ↔ red (+) axis.
    pub a: f64,
    /// Blue (−) ↔ yellow (+) axis.
    pub b: f64,
}

fn srgb_to_linear(c: u8) -> f64 {
    let c = f64::from(c) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(x: f64) -> u8 {
    let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
    let v = if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    // Clamped to 0..=255 before the cast, so truncation cannot occur.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = (v * 255.0).round().clamp(0.0, 255.0) as u8;
    out
}

impl Oklab {
    /// Convert an sRGB colour to OKLab (Björn Ottosson's reference matrices).
    #[must_use]
    pub fn from_rgb(c: RgbColor) -> Self {
        let r = srgb_to_linear(c.r);
        let g = srgb_to_linear(c.g);
        let b = srgb_to_linear(c.b);
        let l = 0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b;
        let m = 0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b;
        let s = 0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b;
        let (l, m, s) = (l.cbrt(), m.cbrt(), s.cbrt());
        Self {
            l: 0.210_454_255_3 * l + 0.793_617_785_0 * m - 0.004_072_046_8 * s,
            a: 1.977_998_495_1 * l - 2.428_592_205_0 * m + 0.450_593_709_9 * s,
            b: 0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766_0 * s,
        }
    }

    /// Convert back to (gamut-clamped) sRGB.
    #[must_use]
    pub fn to_rgb(self) -> RgbColor {
        let l = self.l + 0.396_337_777_4 * self.a + 0.215_803_757_3 * self.b;
        let m = self.l - 0.105_561_345_8 * self.a - 0.063_854_172_8 * self.b;
        let s = self.l - 0.089_484_177_5 * self.a - 1.291_485_548_0 * self.b;
        let (l, m, s) = (l * l * l, m * m * m, s * s * s);
        RgbColor::new(
            linear_to_srgb(4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s),
            linear_to_srgb(-1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s),
            linear_to_srgb(-0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s),
        )
    }

    fn lerp(self, o: Self, t: f64) -> Self {
        Self {
            l: self.l + (o.l - self.l) * t,
            a: self.a + (o.a - self.a) * t,
            b: self.b + (o.b - self.b) * t,
        }
    }
}

/// Interpolate `a → b` in OKLab; `t` is clamped and the endpoints are returned bit-exact.
#[must_use]
pub fn mix_oklab(a: RgbColor, b: RgbColor, t: f64) -> RgbColor {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    if t <= 0.0 || a == b {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    Oklab::from_rgb(a).lerp(Oklab::from_rgb(b), t).to_rgb()
}

/// Sample a 1/2/3-stop gradient at `t` with OKLab interpolation (§7 formula:
/// `t < .5 ? lerp(s, m, 2t) : lerp(m, e, 2t − 1)`). Stop semantics match
/// [`ColorGradient::sample`].
#[must_use]
pub fn sample_oklab(g: &ColorGradient, t: f64) -> RgbColor {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    match (g.mid, g.end) {
        (Some(mid), Some(end)) => {
            if t < 0.5 {
                mix_oklab(g.start, mid, t * 2.0)
            } else {
                mix_oklab(mid, end, t * 2.0 - 1.0)
            }
        }
        (None, Some(end)) => mix_oklab(g.start, end, t),
        (Some(mid), None) => mix_oklab(g.start, mid, t),
        (None, None) => g.start,
    }
}

/// Precompute the 16-entry LUT for a gradient: entry `i` is the colour at `t = i / 15`.
#[must_use]
pub fn gradient_lut(g: &ColorGradient) -> [RgbColor; GRADIENT_STEPS] {
    let mut out = [g.start; GRADIENT_STEPS];
    for (i, slot) in out.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f64 / (GRADIENT_STEPS - 1) as f64;
        *slot = sample_oklab(g, t);
    }
    out
}

/// Quantize `t ∈ [0,1]` (NaN → 0, ±inf clamped) to a LUT index `0..GRADIENT_STEPS`.
#[must_use]
pub fn quantize(t: f32) -> usize {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let i = (t * (GRADIENT_STEPS - 1) as f32).round() as usize;
    i.min(GRADIENT_STEPS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_near_identity() {
        for &(r, g, b) in &[
            (0, 0, 0),
            (255, 255, 255),
            (255, 0, 0),
            (18, 52, 86),
            (250, 189, 47),
        ] {
            let c = RgbColor::new(r, g, b);
            let back = Oklab::from_rgb(c).to_rgb();
            assert!(
                c.r.abs_diff(back.r) <= 1 && c.g.abs_diff(back.g) <= 1 && c.b.abs_diff(back.b) <= 1
            );
        }
    }

    #[test]
    fn quantize_bounds_and_nan() {
        assert_eq!(quantize(0.0), 0);
        assert_eq!(quantize(1.0), 15);
        assert_eq!(quantize(f32::NAN), 0);
        assert_eq!(quantize(f32::INFINITY), 15);
        assert_eq!(quantize(f32::NEG_INFINITY), 0);
        assert_eq!(quantize(0.5), 8);
    }

    #[test]
    fn lut_endpoints_exact() {
        let g = ColorGradient::new_3stop(
            RgbColor::new(0x55, 0x55, 0xFF),
            RgbColor::new(0x55, 0xFF, 0xFF),
            RgbColor::new(0xFF, 0xFF, 0xFF),
        );
        let lut = gradient_lut(&g);
        assert_eq!(lut[0], g.start);
        assert_eq!(lut[15], RgbColor::new(0xFF, 0xFF, 0xFF));
        let solid = gradient_lut(&ColorGradient::solid(RgbColor::new(1, 2, 3)));
        assert!(solid.iter().all(|c| *c == RgbColor::new(1, 2, 3)));
    }
}
