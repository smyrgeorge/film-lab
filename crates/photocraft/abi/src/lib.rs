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
//! to every `pc_filter` call as `_analysis`. [`analyze`] runs the companion's measurement there
//! ([`film_lab_core::measure`], the code `film-lab analyze` runs too); [`Band::measurement`]
//! prefers `_analysis` over the block, so PhotoCraft (which never calls it) behaves as before.
//! The README's `pc_analyze` section is the contract: the signature, the packing and the JSON.
#![no_std]

use film_lab_core::{Analysis, Converter, Encoding, MeasureOptions, Params, Sharpen, Tables, ValueHistogram, params, sharpen};

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
    /// The measurement for a grey document, whichever source it came from: every triple (levels
    /// and balance) takes its first channel, as a grey document's colour parameters take their
    /// first key ([`Band::colour_param`]). Measurements of grey frames have equal channels anyway.
    pub fn grey(self) -> Self {
        let first = |v: [f32; 3]| [v[0]; 3];
        Self { base: first(self.base), black: first(self.black), white: first(self.white), balance: first(self.balance), ..self }
    }

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

/// The histogram (12 KiB) and the JSON `pc_analyze` returns live here: no allocator, and too
/// much for the stack.
static mut HISTOGRAM: ValueHistogram = ValueHistogram::new();
static mut ANALYSIS_OUT: [u8; 256] = [0; 256];

/// JSON written into a fixed byte buffer by hand: `core::fmt`'s float formatting alone would add
/// some 20 KB to each module. `ok` turns false once something didn't fit.
struct Out<'a> {
    buf: &'a mut [u8],
    at: usize,
    ok: bool,
}

impl Out<'_> {
    fn bytes(&mut self, b: &[u8]) {
        match self.at.checked_add(b.len()).and_then(|end| Some((end, self.buf.get_mut(self.at..end)?))) {
            Some((end, dst)) => {
                dst.copy_from_slice(b);
                self.at = end;
            }
            None => self.ok = false,
        }
    }

    fn uint(&mut self, mut v: u32) {
        let mut digits = [0u8; 10];
        let mut i = digits.len();
        loop {
            i -= 1;
            if let Some(d) = digits.get_mut(i) {
                *d = b'0' + (v % 10) as u8;
            }
            v /= 10;
            if v == 0 || i == 0 {
                break;
            }
        }
        self.bytes(digits.get(i..).unwrap_or(&[]));
    }

    /// A non-negative value rounded to `places` (1..=3) decimals; negative or NaN is written as 0.
    fn fixed(&mut self, v: f32, places: u32) {
        let scale = 10u32.pow(places);
        let n = (v * scale as f32 + 0.5) as u32; // saturating: NaN and negatives give 0
        self.uint(n / scale);
        self.bytes(b".");
        let frac = n % scale;
        let mut div = scale / 10;
        while div > 0 {
            self.bytes(&[b'0' + (frac / div % 10) as u8]);
            div /= 10;
        }
    }

    fn triple(&mut self, key: &[u8], v: [f32; 3], places: u32) {
        self.bytes(b",\"");
        self.bytes(key);
        self.bytes(b"\":[");
        for (i, x) in v.into_iter().enumerate() {
            if i > 0 {
                self.bytes(b",");
            }
            self.fixed(x, places);
        }
        self.bytes(b"]");
    }
}

/// `pc_analyze`: the companion's whole-scan measurement ([`film_lab_core::measure`] with
/// [`MeasureOptions::DEFAULT`], the companion's default options) on the frame `band` holds. The
/// values are sRGB-encoded scan values on the 0..255 scale whatever the input encoding, the way
/// the companion reports them. Returns `len << 32 | ptr` of the JSON, or 0 when there is nothing
/// to measure.
pub fn analyze(band: &Band) -> i64 {
    // SAFETY: a wasm instance runs one call at a time, and nothing else reaches the histogram.
    let hist = unsafe { &mut *core::ptr::addr_of_mut!(HISTOGRAM) };
    let (w, n, enc, grey, alpha) = (band.width, band.channels, band.enc, band.grey(), band.alpha);
    let px: &[f32] = band.px;
    let pixel = |x: usize, y: usize| {
        let at = y.checked_mul(w)?.checked_add(x)?.checked_mul(n)?;
        let pix = px.get(at..at.checked_add(n)?)?;
        if !Band::opaque(alpha, n, pix) {
            return None;
        }
        let c0 = *pix.first()?;
        let rgb = if grey { [c0; 3] } else { [c0, *pix.get(1)?, *pix.get(2)?] };
        Some(if enc == Encoding::Srgb { rgb } else { Encoding::Srgb.encode3([enc.decode(rgb[0]), enc.decode(rgb[1]), enc.decode(rgb[2])]) })
    };
    let Ok(m) = film_lab_core::measure(band.width, band.height, &MeasureOptions::DEFAULT, hist, pixel) else { return 0 };

    // SAFETY: as the histogram's.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(ANALYSIS_OUT) };
    let mut out = Out { buf, at: 0, ok: true };
    out.bytes(br#"{"filmlab":1,"scale":"srgb255""#);
    out.triple(b"base", m.base, 1);
    out.triple(b"black", m.black, 1);
    out.triple(b"white", m.white, 1);
    out.triple(b"balance", m.balance, 3);
    out.bytes(br#","samples":"#);
    out.uint(m.samples);
    out.bytes(b"}");
    if !out.ok {
        return 0;
    }
    ((out.at as i64) << 32) | (out.buf.as_ptr() as usize as i64)
}

/// A JSON reader just big enough for `_analysis`, nesting- and escape-aware: no allocator, so no
/// JSON library. Anything it can't read is `None`, and the plug-in falls back as without one.
mod json {
    fn skip_ws(s: &[u8]) -> &[u8] {
        s.get(s.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(s.len())..).unwrap_or(&[])
    }

    /// The length of the string `s` starts with (quotes included; `\"` and `\\` honoured).
    fn string_len(s: &[u8]) -> Option<usize> {
        let mut i = 1usize;
        loop {
            match s.get(i)? {
                b'\\' => i = i.checked_add(2)?,
                b'"' => return i.checked_add(1),
                _ => i = i.checked_add(1)?,
            }
        }
    }

    /// The length of the value `s` starts with: a string, an object or array (strings and nesting
    /// inside it honoured), or a scalar up to the next delimiter.
    fn value_len(s: &[u8]) -> Option<usize> {
        match s.first()? {
            b'"' => string_len(s),
            b'{' | b'[' => {
                let (mut depth, mut i) = (0usize, 0usize);
                while let Some(b) = s.get(i) {
                    match b {
                        b'"' => {
                            i = i.checked_add(string_len(s.get(i..)?)?)?;
                            continue;
                        }
                        b'{' | b'[' => depth = depth.checked_add(1)?,
                        b'}' | b']' => {
                            depth = depth.checked_sub(1)?;
                            if depth == 0 {
                                return i.checked_add(1);
                            }
                        }
                        _ => {}
                    }
                    i = i.checked_add(1)?;
                }
                None
            }
            _ => Some(s.iter().position(|b| matches!(b, b',' | b'}' | b']') || b.is_ascii_whitespace()).unwrap_or(s.len())),
        }
    }

    /// The value of the top-level member `key` of the object `obj` (nested objects and strings are
    /// skipped whole, so a key inside them never matches).
    pub fn member<'a>(obj: &'a [u8], key: &str) -> Option<&'a [u8]> {
        let mut s = skip_ws(obj).strip_prefix(b"{")?;
        loop {
            s = skip_ws(s);
            if s.first() == Some(&b'}') {
                return None;
            }
            let klen = string_len(s).filter(|_| s.first() == Some(&b'"'))?;
            let k = s.get(1..klen - 1)?;
            s = skip_ws(s.get(klen..)?).strip_prefix(b":")?;
            s = skip_ws(s);
            let vlen = value_len(s)?;
            if k == key.as_bytes() {
                return s.get(..vlen);
            }
            s = skip_ws(s.get(vlen..)?);
            s = s.strip_prefix(b",").unwrap_or(s);
        }
    }

    /// The number under `key` in `obj`, if it is one and finite.
    pub fn number(obj: &[u8], key: &str) -> Option<f32> {
        core::str::from_utf8(member(obj, key)?).ok()?.parse::<f32>().ok().filter(|v| v.is_finite())
    }

    /// The three finite numbers of the array under `key` in `obj` (`[r, g, b]`).
    pub fn triple(obj: &[u8], key: &str) -> Option<[f32; 3]> {
        let a = member(obj, key)?;
        let inner = core::str::from_utf8(a.strip_prefix(b"[")?.strip_suffix(b"]")?).ok()?;
        let mut v = [0.0f32; 3];
        let mut it = inner.split(',');
        for x in v.iter_mut() {
            *x = it.next()?.trim().parse::<f32>().ok().filter(|f| f.is_finite())?;
        }
        if it.next().is_some() { None } else { Some(v) }
    }
}

/// A measurement from the host's `_analysis` (the object [`analyze`] returned), if `params` has
/// one: `filmlab` 1, `scale` `"srgb255"`, and `base`, `black`, `white` (and optionally `balance`)
/// as `[r, g, b]`. Other members are ignored.
fn host_measurement(params: &[u8]) -> Option<Measurement> {
    let a = json::member(params, "_analysis")?;
    if json::number(a, "filmlab")? != 1.0 || json::member(a, "scale")? != br#""srgb255""# {
        return None;
    }
    let s = |v: [f32; 3]| [v[0] / 255.0, v[1] / 255.0, v[2] / 255.0];
    Some(Measurement {
        base: s(json::triple(a, "base")?),
        black: s(json::triple(a, "black")?),
        white: s(json::triple(a, "white")?),
        balance: json::triple(a, "balance").unwrap_or([1.0; 3]),
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
        let m = host_measurement(self.json).or_else(|| {
            let b = measurement_block();
            let base = colour(b, ["base_r", "base_g", "base_b"], false)?;
            let black = colour(b, ["black_r", "black_g", "black_b"], false).unwrap_or(base);
            let white = colour(b, ["white_r", "white_g", "white_b"], false)?;
            let bal = |k: &str| params::number(b, k).unwrap_or(1.0);
            Some(Measurement { base, black, white, balance: [bal("balance_r"), bal("balance_g"), bal("balance_b")], encoding: None })
        })?;
        Some(if self.grey() { m.grey() } else { m })
    }

    /// Levels from a film base alone (scan values in the document's encoding), for a scan of the
    /// given channel gammas: the same on every band. A grey document's one channel takes the
    /// green gamma, as its base takes the green base.
    pub fn from_base(&self, base: [f32; 3], film_gamma: [f32; 3]) -> Analysis {
        let enc = self.enc;
        let film_gamma = if self.grey() { [film_gamma[1]; 3] } else { film_gamma };
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
