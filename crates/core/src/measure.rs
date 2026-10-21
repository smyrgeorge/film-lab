//! The whole-scan measurement: the film base, each channel's black and white points and the
//! neutral balance, from a strided sample of the frame.
//!
//! One implementation for every host that sees the whole frame: the companion
//! (`film-lab-companion`'s `analyze`, for PhotoCraft, which hands its plug-ins bands) and a
//! host's call of the plug-ins' `pc_analyze`. The caller supplies the pixels and the histogram;
//! the results are sRGB-encoded scan values on the 0..255 scale the plug-in's colour parameters
//! use.

use crate::{Analysis, Converter, Encoding, Params, Tone, ValueHistogram, color};

/// How the measurement treats the scan; [`MeasureOptions::DEFAULT`] is the companion's default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasureOptions {
    /// Percentage of each edge left out (film holder, sprocket holes, the border itself), 0..45.
    pub border_percent: f32,
    /// Share of the brightest samples that may lie above the film base (dust, hot pixels), %.
    pub base_clip: f32,
    /// Share of samples allowed below black and above white, %.
    pub black_clip: f32,
    pub white_clip: f32,
    /// Bring each channel's median to the common median ([`neutral_balance`]); otherwise the
    /// balance is `[1, 1, 1]`.
    pub neutral: bool,
    /// At most this many pixels are sampled (a regular grid over the frame).
    pub max_samples: usize,
}

impl MeasureOptions {
    pub const DEFAULT: Self = Self { border_percent: 8.0, base_clip: 0.1, black_clip: 0.3, white_clip: 0.3, neutral: true, max_samples: 1_000_000 };
}

impl Default for MeasureOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What the measurement found: sRGB-encoded scan values on the 0..255 scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measured {
    pub base: [f32; 3],
    pub black: [f32; 3],
    pub white: [f32; 3],
    /// Per-channel mid-tone gammas that neutralise the scene average (1 = none).
    pub balance: [f32; 3],
    /// Pixels the measurement looked at.
    pub samples: u32,
}

/// Why there is nothing to measure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeasureError {
    /// The border takes the whole frame.
    NoArea,
    /// Every sampled pixel was skipped (transparent).
    NoPixels,
    /// A channel has no range between its black and white points.
    Flat { channel: usize },
}

impl core::fmt::Display for MeasureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoArea => f.write_str("the border leaves no pixels to analyse"),
            Self::NoPixels => f.write_str("no pixels to analyse"),
            Self::Flat { channel } => {
                write!(f, "channel {channel} has no tonal range between black and white (a flat scan, or the border buffer is too small)")
            }
        }
    }
}

/// Measures a `width × height` frame. `pixel(x, y)` gives the sRGB-encoded sample (0..1) at a
/// grid point, or `None` to skip it (a transparent pixel); `hist` is the caller's, so a host
/// without an allocator can keep it in a static.
pub fn measure(
    width: usize,
    height: usize,
    o: &MeasureOptions,
    hist: &mut ValueHistogram,
    mut pixel: impl FnMut(usize, usize) -> Option<[f32; 3]>,
) -> Result<Measured, MeasureError> {
    *hist = ValueHistogram::new();
    let border = f64::from(if o.border_percent.is_finite() { o.border_percent.clamp(0.0, 45.0) } else { 0.0 }) / 100.0;
    let (bx, by) = ((width as f64 * border) as usize, (height as f64 * border) as usize);
    let (x0, x1, y0, y1) = (bx, width.saturating_sub(bx), by, height.saturating_sub(by));
    if x1 <= x0 || y1 <= y0 {
        return Err(MeasureError::NoArea);
    }
    let area = (x1 - x0).saturating_mul(y1 - y0);
    let step = (libm::ceil(libm::sqrt(area as f64 / o.max_samples.max(1) as f64)) as usize).max(1);
    for y in (y0..y1).step_by(step) {
        for x in (x0..x1).step_by(step) {
            if let Some(rgb) = pixel(x, y) {
                hist.push(rgb);
            }
        }
    }
    if hist.count() == 0 {
        return Err(MeasureError::NoPixels);
    }
    let clip = |pct: f32| if pct.is_finite() { (pct / 100.0).clamp(0.0, 0.5) } else { 0.0 };
    let q = |ch: usize, frac: f32| hist.quantile(ch, frac).map(|v| v * 255.0).ok_or(MeasureError::NoPixels);
    let mut m = Measured { base: [0.0; 3], black: [0.0; 3], white: [0.0; 3], balance: [1.0; 3], samples: hist.count() };
    let mut median = [0.0; 3];
    for (ch, med) in median.iter_mut().enumerate() {
        m.base[ch] = q(ch, 1.0 - clip(o.base_clip))?;
        m.black[ch] = q(ch, 1.0 - clip(o.black_clip))?;
        m.white[ch] = q(ch, clip(o.white_clip))?;
        *med = q(ch, 0.5)?;
        if m.white[ch] >= m.black[ch] {
            return Err(MeasureError::Flat { channel: ch });
        }
    }
    if o.neutral {
        m.balance = neutral_balance(&m, median);
    }
    Ok(m)
}

/// Per-channel gammas that bring each channel's median positive to the geometric mean of the
/// three: the scene averages grey, black and white stay put. Values are sRGB scan values, 0..255.
pub fn neutral_balance(m: &Measured, median_scan: [f32; 3]) -> [f32; 3] {
    let enc = Encoding::Srgb;
    let scale = |v: [f32; 3]| [v[0] / 255.0, v[1] / 255.0, v[2] / 255.0];
    let Some(a) = Analysis::from_scan_values(scale(m.base), scale(m.black), scale(m.white), enc) else { return [1.0; 3] };
    let c = Converter::new(&a, &Params { tone: Tone::Linear, ..Params::default() }, enc, None);
    let out = c.convert(scale(median_scan));
    let m =
        [color::srgb_decode(out[0]).clamp(1e-3, 0.999), color::srgb_decode(out[1]).clamp(1e-3, 0.999), color::srgb_decode(out[2]).clamp(1e-3, 0.999)];
    let target = (libm::logf(m[0]) + libm::logf(m[1]) + libm::logf(m[2])) / 3.0;
    let mut balance = [1.0; 3];
    for (b, v) in balance.iter_mut().zip(m) {
        *b = (target / libm::logf(v)).clamp(0.4, 2.5);
    }
    balance
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::film;

    const BASE: [f32; 3] = [0.75, 0.45, 0.28];

    /// A negative ramp from the base (scene black) to 2⁵ brighter, with a different scene colour
    /// per channel, inside a border of pure base.
    fn negative(w: usize, h: usize) -> impl Fn(usize, usize) -> Option<[f32; 3]> {
        move |x, y| {
            let edge = x < w / 10 || y < h / 10 || x >= w - w / 10 || y >= h - h / 10;
            let t = (x + y * w) as f32 / (w * h) as f32;
            let mut rgb = [0.0; 3];
            for (ch, (v, b)) in rgb.iter_mut().zip(BASE).enumerate() {
                let e = if edge { 1.0 } else { libm::exp2f(5.0 * t * (0.8 + 0.2 * ch as f32)) };
                *v = color::srgb_encode(film::transmittance(e, b));
            }
            Some(rgb)
        }
    }

    #[test]
    fn measures_a_negative_and_neutralises_it() {
        let mut hist = ValueHistogram::new();
        let m = measure(400, 300, &MeasureOptions::DEFAULT, &mut hist, negative(400, 300)).unwrap();
        for ch in 0..3 {
            assert!(m.white[ch] < m.black[ch] && m.black[ch] <= m.base[ch], "{m:?}");
        }
        assert!(m.samples > 0 && m.balance != [1.0; 3], "{m:?}");
        let auto = measure(400, 300, &MeasureOptions { neutral: false, ..MeasureOptions::DEFAULT }, &mut hist, negative(400, 300)).unwrap();
        assert_eq!(auto.balance, [1.0; 3]);
        assert_eq!((auto.base, auto.black, auto.white), (m.base, m.black, m.white));
    }

    #[test]
    fn caps_the_samples() {
        let mut hist = ValueHistogram::new();
        let o = MeasureOptions { max_samples: 1000, ..MeasureOptions::DEFAULT };
        let m = measure(2000, 1000, &o, &mut hist, negative(2000, 1000)).unwrap();
        assert!(m.samples <= 1100, "{m:?}");
    }

    #[test]
    fn reports_why_nothing_was_measured() {
        let mut hist = ValueHistogram::new();
        let o = MeasureOptions::DEFAULT;
        assert_eq!(measure(0, 0, &o, &mut hist, |_, _| Some([0.5; 3])), Err(MeasureError::NoArea));
        assert_eq!(measure(10, 0, &MeasureOptions { border_percent: 50.0, ..o }, &mut hist, |_, _| Some([0.5; 3])), Err(MeasureError::NoArea));
        assert_eq!(measure(10, 10, &o, &mut hist, |_, _| None), Err(MeasureError::NoPixels));
        assert_eq!(measure(10, 10, &o, &mut hist, |_, _| Some([0.5; 3])), Err(MeasureError::Flat { channel: 0 }));
        // Hostile options and samples still give an answer or an error, never a panic.
        let wild = MeasureOptions {
            border_percent: f32::NAN,
            base_clip: f32::INFINITY,
            black_clip: -5.0,
            white_clip: f32::NAN,
            neutral: true,
            max_samples: 0,
        };
        let _ = measure(64, 64, &wild, &mut hist, |x, _| Some([x as f32 / 32.0 - 0.5, f32::NAN, f32::INFINITY]));
    }
}
