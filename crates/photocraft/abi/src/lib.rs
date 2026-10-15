//! What the Film Lab PhotoCraft plug-ins share: the PhotoCraft WebAssembly plug-in ABI v1
//! (`photocraft/docs/plugins.md`; built like `photocraft/examples/plugins/invert-rs`).
//!
//! `no_std`, no allocator, no imports. An ABI v1 module is one manifest and one menu entry, so
//! each plug-in is its own `cdylib` crate: its manifest, and a function that reads its parameters
//! into [`film_lab_core`] settings. [`plugin!`] writes the four exports around them; [`Band`] is
//! the layer band the host hands over, and converts it in place.
//!
//! # The measurement
//!
//! What belongs to the scan rather than the film (the film base, the black and white points,
//! a balance) has to be measured on the whole image: PhotoCraft hands a plug-in the layer in
//! bands of 256 rows in fresh instances with no whole-image pass, the dialog's preview included,
//! so nothing measured from one band agrees with the next. The companion measures the whole scan
//! and writes the result into the module before installing it ([`MEASUREMENT_MARKER`]); every
//! plug-in reads it with [`Band::measurement`].
//!
//! # `pc_analyze` (ABI v1.1 proposal)
//!
//! A host with a whole-image pass (LightCraft Classic's pixel filters) calls the optional
//! `pc_analyze(buf, len, w, h, channels, format, params, params_len) -> i64` once on a whole,
//! downsampled frame and passes the JSON object it returns (`len << 32 | ptr`, 0 = nothing) back
//! to every `pc_filter` call as `_analysis`. [`analyze`] runs the companion's measurement there;
//! [`Band::measurement`] prefers `_analysis` over the block, so PhotoCraft (which never calls it)
//! behaves as before.
#![no_std]

use film_lab_core::{Analysis, Converter, Encoding, Params, Sharpen, Tables, ValueHistogram, params, sharpen};

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

/// Writes the four ABI v1 exports of a plug-in crate: `pc_abi_version`, `pc_manifest` (the
/// `&'static str` given), `pc_alloc`, and `pc_filter`, which hands the band to the
/// `fn(Band) -> i32` given and returns its code ([`OK`], or a code the host reports as an error).
#[macro_export]
macro_rules! plugin {
    ($manifest:expr, $filter:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn pc_abi_version() -> u32 {
            1
        }

        /// `len << 32 | ptr` of the UTF-8 manifest JSON.
        #[unsafe(no_mangle)]
        pub extern "C" fn pc_manifest() -> u64 {
            let m: &'static str = $manifest;
            ((m.len() as u64) << 32) | m.as_ptr() as u64
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn pc_alloc(size: u32) -> u32 {
            $crate::alloc(size)
        }

        /// Converts `width × height` pixels of `channels` interleaved `f32` samples at `buf` in place.
        ///
        /// # Safety
        /// The host guarantees `buf..buf + len` is a block it got from `pc_alloc` holding `len / 4`
        /// samples, and `params..params + params_len` the UTF-8 parameter JSON.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pc_filter(
            buf: *mut f32,
            len: u32,
            width: u32,
            height: u32,
            channels: u32,
            format: u32,
            params: *const u8,
            params_len: u32,
        ) -> i32 {
            // SAFETY: the host's contract above is `Band::new`'s.
            match unsafe { $crate::Band::new(buf, len, width, height, channels, format, params, params_len) } {
                Ok(band) => $filter(band),
                Err(code) => code,
            }
        }

        /// ABI v1.1 proposal: measures the whole frame once; returns `len << 32 | ptr` of a JSON
        /// object for the host to pass back to `pc_filter` as `_analysis`, or 0 for nothing.
        ///
        /// # Safety
        /// As `pc_filter`'s; the buffer is only read.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn pc_analyze(
            buf: *mut f32,
            len: u32,
            width: u32,
            height: u32,
            channels: u32,
            format: u32,
            params: *const u8,
            params_len: u32,
        ) -> i64 {
            // SAFETY: the host's contract above is `Band::new`'s.
            match unsafe { $crate::Band::new(buf, len, width, height, channels, format, params, params_len) } {
                Ok(band) => $crate::analyze(&band),
                Err(_) => 0,
            }
        }
    };
}

/// Return codes of `pc_filter` (anything but 0 is reported by the host as an error).
pub const OK: i32 = 0;
pub const ERR_NO_CHANNELS: i32 = 1;
pub const ERR_UNSUPPORTED_MODE: i32 = 2;

/// Bit 8 of the `format` argument: the last channel is alpha.
const HAS_ALPHA: u32 = 1 << 8;

/// A block of `size` bytes: fresh pages from `memory.grow` (each run gets a fresh instance, so
/// nothing is ever freed). Returns 0 when the host's memory cap is reached.
pub fn alloc(size: u32) -> u32 {
    let pages = (size as usize).div_ceil(65536);
    let old = core::arch::wasm32::memory_grow(0, pages);
    if old == usize::MAX { 0 } else { (old * 65536) as u32 }
}

/// Starts the measurement block the companion fills in: the marker, then JSON padded with
/// spaces to [`MEASUREMENT_LEN`] bytes in all, with the keys `base_r/g/b`, `black_r/g/b`,
/// `white_r/g/b` (scan values, 0..255) and `balance_r/g/b`.
pub const MEASUREMENT_MARKER: &[u8] = b"FILMLAB-MEASURE-1:";
pub const MEASUREMENT_LEN: usize = 512;

const fn blank_block() -> [u8; MEASUREMENT_LEN] {
    let mut b = [b' '; MEASUREMENT_LEN];
    let mut i = 0;
    while i < MEASUREMENT_MARKER.len() {
        b[i] = MEASUREMENT_MARKER[i];
        i += 1;
    }
    b
}

/// The block lives in the module's data segment; `static mut` keeps the optimiser from folding
/// its initial (blank) contents into the code that reads it.
#[used]
static mut MEASUREMENT: [u8; MEASUREMENT_LEN] = blank_block();

/// The conversion's lookup tables, refilled on every call: 48 KiB, too much for the stack, and
/// what keeps a sample to a few dozen instructions under the host's interpreter.
static mut TABLES: Tables = Tables::blank();

fn measurement_block() -> &'static [u8] {
    // SAFETY: the block is only ever read, and only through this shared reference.
    let b = unsafe { &*core::ptr::addr_of!(MEASUREMENT) };
    b.get(MEASUREMENT_MARKER.len()..).unwrap_or(&[])
}

/// A colour given as the Info panel shows it (0..255), as a host-encoded triple, or `None`
/// when unset (all 0). A grey document has one channel: the first key stands for it.
fn colour(json: &[u8], keys: [&str; 3], grey: bool) -> Option<[f32; 3]> {
    let v = |k: &str| params::number(json, k).map_or(0.0, |x| x / 255.0);
    let mut c = [v(keys[0]), v(keys[1]), v(keys[2])];
    if grey {
        c = [c[0]; 3];
    }
    if c == [0.0; 3] { None } else { Some(c) }
}

/// The companion's measurement as the block holds it: scan values in the host encoding (0..1)
/// and the per-channel balance. One from `_analysis` holds sRGB-encoded scan values whatever the
/// band's encoding (`encoding` is then `Some(Srgb)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    pub base: [f32; 3],
    pub black: [f32; 3],
    pub white: [f32; 3],
    pub balance: [f32; 3],
    /// The encoding the scan values are in; `None`: the band's.
    pub encoding: Option<Encoding>,
}

impl Measurement {
    /// The levels these scan values give, or `None` when a channel has no range. `enc` is the
    /// band's encoding, used unless the measurement names its own.
    pub fn analysis(&self, enc: Encoding) -> Option<Analysis> {
        Analysis::from_scan_values(self.base, self.black, self.white, self.encoding.unwrap_or(enc))
    }

    /// A scan value in the band's encoding `enc` (0..1) re-encoded the way this measurement's
    /// values are (an Eyedropper pick to merge with an `_analysis` measurement).
    pub fn reencode(&self, enc: Encoding, v: [f32; 3]) -> [f32; 3] {
        match self.encoding {
            Some(m) if m != enc => m.encode3([enc.decode(v[0]), enc.decode(v[1]), enc.decode(v[2])]),
            _ => v,
        }
    }
}

/// The companion's defaults (`AnalyzeOptions`): each edge's share left out, and the shares of the
/// brightest samples above the base and of samples below black / above white.
const BORDER: f32 = 0.05;
const BASE_CLIP: f32 = 0.001;
const BLACK_CLIP: f32 = 0.003;
const WHITE_CLIP: f32 = 0.003;
/// Samples the measurement looks at, at most (a regular grid).
const MAX_SAMPLES: usize = 1_000_000;

/// The histogram (12 KiB) and the JSON `pc_analyze` returns live here: no allocator, and too
/// much for the stack.
static mut HISTOGRAM: ValueHistogram = ValueHistogram::new();
static mut ANALYSIS_OUT: [u8; 512] = [0; 512];

/// A `core::fmt::Write` into a fixed byte buffer.
struct Out<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl core::fmt::Write for Out<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.at.checked_add(s.len()).ok_or(core::fmt::Error)?;
        self.buf.get_mut(self.at..end).ok_or(core::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.at = end;
        Ok(())
    }
}

/// `pc_analyze`: the companion's whole-scan measurement (`film-lab-companion`'s `analyze` with
/// its default options) on the frame `band` holds. Values are sRGB-encoded scan values on the
/// 0..255 scale whatever the input encoding, the way the companion reports them. Returns
/// `len << 32 | ptr` of the JSON, or 0 when there is nothing to measure.
pub fn analyze(band: &Band) -> i64 {
    // SAFETY: a wasm instance runs one call at a time, and nothing else reaches the histogram.
    let hist = unsafe { &mut *core::ptr::addr_of_mut!(HISTOGRAM) };
    *hist = ValueHistogram::new();
    let (w, h, n) = (band.width, band.height, band.channels);
    let (bx, by) = ((w as f32 * BORDER) as usize, (h as f32 * BORDER) as usize);
    let (x0, x1, y0, y1) = (bx, w.saturating_sub(bx), by, h.saturating_sub(by));
    if x1 <= x0 || y1 <= y0 || n == 0 {
        return 0;
    }
    let area = (x1 - x0) * (y1 - y0);
    let step = (libm::ceilf(libm::sqrtf(area as f32 / MAX_SAMPLES as f32)) as usize).max(1);
    let enc = band.enc;
    for y in (y0..y1).step_by(step) {
        for x in (x0..x1).step_by(step) {
            let Some(at) = y.checked_mul(w).and_then(|i| i.checked_add(x)).and_then(|i| i.checked_mul(n)) else { continue };
            let Some(pix) = band.px.get(at..at.saturating_add(n)) else { continue };
            if !Band::opaque(band.alpha, n, pix) {
                continue;
            }
            let c0 = pix.first().copied().unwrap_or(0.0);
            let rgb = if band.grey() { [c0; 3] } else { [c0, pix.get(1).copied().unwrap_or(c0), pix.get(2).copied().unwrap_or(c0)] };
            hist.push(Encoding::Srgb.encode3([enc.decode(rgb[0]), enc.decode(rgb[1]), enc.decode(rgb[2])]));
        }
    }
    let q = |ch: usize, f: f32| hist.quantile(ch, f).map(|v| v * 255.0);
    let (mut base, mut black, mut white) = ([0.0f32; 3], [0.0f32; 3], [0.0f32; 3]);
    for ch in 0..3 {
        let (Some(b), Some(k), Some(wh)) = (q(ch, 1.0 - BASE_CLIP), q(ch, 1.0 - BLACK_CLIP), q(ch, WHITE_CLIP)) else { return 0 };
        if wh >= k {
            return 0; // no tonal range: a flat frame
        }
        (base[ch], black[ch], white[ch]) = (b, k, wh);
    }
    // SAFETY: as the histogram's.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(ANALYSIS_OUT) };
    let mut out = Out { buf, at: 0 };
    let r = |v: f32| libm::roundf(v * 10.0) / 10.0;
    let ok = core::fmt::write(
        &mut out,
        format_args!(
            r#"{{"filmlab":1,"scale":"srgb255","base":[{},{},{}],"black":[{},{},{}],"white":[{},{},{}],"balance":[1,1,1],"samples":{}}}"#,
            r(base[0]),
            r(base[1]),
            r(base[2]),
            r(black[0]),
            r(black[1]),
            r(black[2]),
            r(white[0]),
            r(white[1]),
            r(white[2]),
            hist.count()
        ),
    );
    if ok.is_err() {
        return 0;
    }
    ((out.at as i64) << 32) | (out.buf.as_ptr() as usize as i64)
}

/// The object under the top-level key `"key"` (from `{` to its matching `}`), if there is one.
fn object<'a>(json: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let needle_len = key.len().checked_add(2)?;
    let mut from = 0;
    loop {
        let i = json
            .get(from..)?
            .windows(needle_len)
            .position(|w| w.first() == Some(&b'"') && w.last() == Some(&b'"') && w.get(1..needle_len - 1) == Some(key.as_bytes()))?;
        let after = from.checked_add(i)?.checked_add(needle_len)?;
        let rest = json.get(after..)?;
        let rest = rest.get(rest.iter().position(|b| !b.is_ascii_whitespace())?..)?;
        if let Some(r) = rest.strip_prefix(b":") {
            let start = r.iter().position(|b| !b.is_ascii_whitespace())?;
            let r = r.get(start..)?;
            if r.first() != Some(&b'{') {
                return None;
            }
            let mut depth = 0usize;
            let mut in_str = false;
            for (j, b) in r.iter().enumerate() {
                match (*b, in_str) {
                    (b'"', _) => in_str = !in_str,
                    (b'{', false) => depth += 1,
                    (b'}', false) => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return r.get(..=j);
                        }
                    }
                    _ => {}
                }
            }
            return None;
        }
        from = after;
    }
}

/// The three numbers of the array under `"key"` in `json` (`[r, g, b]`).
fn triple(json: &[u8], key: &str) -> Option<[f32; 3]> {
    let needle_len = key.len().checked_add(2)?;
    let i = json
        .windows(needle_len)
        .position(|w| w.first() == Some(&b'"') && w.last() == Some(&b'"') && w.get(1..needle_len - 1) == Some(key.as_bytes()))?;
    let rest = json.get(i.checked_add(needle_len)?..)?;
    let open = rest.iter().position(|b| *b == b'[')?;
    let close = rest.iter().position(|b| *b == b']')?;
    let inner = core::str::from_utf8(rest.get(open.checked_add(1)?..close)?).ok()?;
    let mut v = [0.0f32; 3];
    let mut it = inner.split(',');
    for x in v.iter_mut() {
        *x = it.next()?.trim().parse::<f32>().ok().filter(|f| f.is_finite())?;
    }
    if it.next().is_some() { None } else { Some(v) }
}

/// A measurement from the host's `_analysis` (what [`analyze`] returned), if `json` has one.
fn host_measurement(json: &[u8], grey: bool) -> Option<Measurement> {
    let a = object(json, "_analysis")?;
    if params::number(a, "filmlab")? != 1.0 {
        return None;
    }
    let s = |v: [f32; 3]| {
        let v = [v[0] / 255.0, v[1] / 255.0, v[2] / 255.0];
        if grey { [v[1]; 3] } else { v }
    };
    let balance = triple(a, "balance").unwrap_or([1.0; 3]);
    Some(Measurement {
        base: s(triple(a, "base")?),
        black: s(triple(a, "black")?),
        white: s(triple(a, "white")?),
        balance: if grey { [1.0; 3] } else { balance },
        encoding: Some(Encoding::Srgb),
    })
}

/// One band of the layer, as the host hands it to `pc_filter`.
pub struct Band<'a> {
    /// `width × height × channels` interleaved samples, margins included.
    pub px: &'a mut [f32],
    /// The parameter JSON: the manifest's parameters, validated, and `_image`.
    pub json: &'a [u8],
    pub width: usize,
    pub height: usize,
    /// Samples per pixel.
    pub channels: usize,
    /// Colour samples per pixel, 1 (grey) or 3 (RGB); the rest is alpha.
    pub colour: usize,
    pub alpha: bool,
    /// The encoding the samples are in: the `encoding` parameter, or the document's depth.
    pub enc: Encoding,
}

impl Band<'_> {
    /// The band `pc_filter` was called with, or the code to return: [`ERR_NO_CHANNELS`] for an
    /// alpha-only band, [`ERR_UNSUPPORTED_MODE`] for CMYK and multichannel documents.
    ///
    /// # Safety
    /// `buf..buf + len` holds `len / 4` samples and `params..params + params_len` the parameter
    /// JSON, both valid for the rest of the call and referenced by nothing else.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new(
        buf: *mut f32,
        len: u32,
        width: u32,
        height: u32,
        channels: u32,
        format: u32,
        params: *const u8,
        params_len: u32,
    ) -> Result<Self, i32> {
        let n = channels as usize;
        let alpha = format & HAS_ALPHA != 0;
        let colour = if alpha { n.saturating_sub(1) } else { n };
        if colour == 0 {
            return Err(ERR_NO_CHANNELS);
        }
        if colour != 1 && colour != 3 {
            // CMYK / multichannel documents: not a negative we know how to read.
            return Err(ERR_UNSUPPORTED_MODE);
        }
        // SAFETY: see the function's contract.
        let px = unsafe { core::slice::from_raw_parts_mut(buf, len as usize / 4) };
        // SAFETY: see the function's contract.
        let json = unsafe { core::slice::from_raw_parts(params, params_len as usize) };
        let enc = params::string(json, "encoding").and_then(Encoding::parse).unwrap_or_else(|| Encoding::for_depth(format & 0xff));
        Ok(Self { px, json, width: width as usize, height: height as usize, channels: n, colour, alpha, enc })
    }

    /// One colour channel: a grey document, read as a neutral triple.
    pub fn grey(&self) -> bool {
        self.colour == 1
    }

    /// A colour parameter given as the Info panel shows it (`base_r/g/b`, `neutral_r/g/b`:
    /// 0..255 in the document's encoding), or `None` when unset (all 0).
    pub fn colour_param(&self, keys: [&str; 3]) -> Option<[f32; 3]> {
        colour(self.json, keys, self.grey())
    }

    /// A `choice` parameter through `table` (the option's text as the manifest lists it, and
    /// what it stands for), or `None` when unset or not listed. The host only passes listed
    /// options, so the dialog's text can be words for people rather than the core's names.
    pub fn choice<T: Copy>(&self, key: &str, table: &[(&str, T)]) -> Option<T> {
        let v = params::string(self.json, key)?;
        table.iter().find(|(text, _)| *text == v).map(|(_, t)| *t)
    }

    /// The whole-image measurement: the host's `_analysis` ([`analyze`]'s result) when the
    /// parameters carry one, else the companion's, when the module holds one.
    pub fn measurement(&self) -> Option<Measurement> {
        if let Some(m) = host_measurement(self.json, self.grey()) {
            return Some(m);
        }
        let b = measurement_block();
        let grey = self.grey();
        let base = colour(b, ["base_r", "base_g", "base_b"], grey)?;
        let black = colour(b, ["black_r", "black_g", "black_b"], grey).unwrap_or(base);
        let white = colour(b, ["white_r", "white_g", "white_b"], grey)?;
        let bal = |k: &str| params::number(b, k).unwrap_or(1.0);
        let balance = if grey { [bal("balance_r"); 3] } else { [bal("balance_r"), bal("balance_g"), bal("balance_b")] };
        Some(Measurement { base, black, white, balance, encoding: None })
    }

    /// Levels from a film base alone (scan values in the document's encoding), for a film of the
    /// given layer gammas: the same on every band.
    pub fn from_base(&self, base: [f32; 3], film_gamma: [f32; 3]) -> Analysis {
        let enc = self.enc;
        Analysis::from_base([enc.decode(base[0]), enc.decode(base[1]), enc.decode(base[2])], film_gamma)
    }

    /// Whether a pixel is converted: alpha 0 means nothing is there, and its values stay.
    fn opaque(alpha: bool, n: usize, pix: &[f32]) -> bool {
        !alpha || pix.get(n - 1).is_some_and(|a| *a > 0.0)
    }

    /// Converts the band in place, then sharpens it when `p` asks for it (the manifest then needs
    /// an `overlap` of 1, for seamless bands). A grey document is converted as monochrome.
    pub fn convert(&mut self, analysis: &Analysis, p: &Params, neutral: Option<[f32; 3]>) {
        let mut p = *p;
        if self.grey() {
            p.monochrome = true;
        }
        let converter = Converter::new(analysis, &p, self.enc, neutral);
        // SAFETY: a wasm instance runs one call at a time, and nothing else reaches the tables.
        let tables = unsafe { &mut *core::ptr::addr_of_mut!(TABLES) };
        converter.tables(tables);

        let (n, colour, alpha) = (self.channels, self.colour, self.alpha);
        // Grayscale (one colour channel) is read as a neutral triple.
        let rgb = |pix: &[f32]| {
            let c0 = pix.first().copied().unwrap_or(0.0);
            [c0, pix.get(1).copied().unwrap_or(c0), pix.get(2).copied().unwrap_or(c0)]
        };
        for pix in self.px.chunks_exact_mut(n) {
            if !Self::opaque(alpha, n, pix) {
                continue;
            }
            let out = converter.convert_with(tables, rgb(pix));
            for (v, o) in pix.iter_mut().take(colour).zip(out) {
                *v = o;
            }
        }

        if p.sharpen != Sharpen::Off && p.sharpen_amount > 0.0 {
            self.sharpen(p.sharpen, p.sharpen_amount);
        }
    }

    /// An unsharp mask on the converted band, over a scratch block grown for it; left unsharpened
    /// when the host's memory cap is reached.
    fn sharpen(&mut self, mode: Sharpen, amount: f32) {
        let Some(count) = self.width.checked_mul(self.height) else { return };
        let Ok(bytes) = u32::try_from(count.saturating_mul(4)) else { return };
        let at = alloc(bytes);
        if at == 0 {
            return;
        }
        // SAFETY: `alloc` returned a fresh block of `bytes` bytes of this module's memory that
        // nothing else references; `f32` has no invalid bit patterns and new pages are zeroed.
        let scratch = unsafe { core::slice::from_raw_parts_mut(at as *mut f32, count) };
        let (n, alpha) = (self.channels, self.alpha);
        let layout = sharpen::Layout { width: self.width, height: self.height, channels: n, colour: self.colour };
        sharpen::unsharp(self.px, scratch, layout, mode, amount, |pix| Self::opaque(alpha, n, pix));
    }
}
