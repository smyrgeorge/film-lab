//! The companion to the PhotoCraft plug-in: what a plug-in band can't do on its own.
//!
//! PhotoCraft runs a filter plug-in on bands of the layer with no whole-image pass, so the
//! plug-in cannot measure the scan itself. This crate analyses the whole scan up front, as
//! a lab-style converter does, and hands the plug-in the result as parameters (`base_*`,
//! `black_*`, `white_*`), which are identical on every band. It then drives `photocraft-cli` to
//! run the plug-in on one scan or a whole roll with one shared analysis.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub mod app;
pub mod compare;

use film_lab_core::{MeasureOptions, ValueHistogram};
use image::DynamicImage;
use serde_json::{Map, Value, json};

/// One Film Lab plug-in module (`crates/photocraft`): an ABI v1 module is one menu entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Plugin {
    /// Film Lab: Convert Negative, a preset and six sliders.
    #[default]
    Convert,
    /// Film Lab: Convert Negative (Advanced), with the film base picked from the scan and every
    /// tone, colour and sharpening control.
    Advanced,
}

impl Plugin {
    pub const ALL: [Plugin; 2] = [Plugin::Convert, Plugin::Advanced];

    /// Its id in PhotoCraft.
    pub fn id(self) -> &'static str {
        match self {
            Plugin::Convert => "io.github.smyrgeorge.film-lab.convert",
            Plugin::Advanced => "io.github.smyrgeorge.film-lab.convert-advanced",
        }
    }

    /// The module's file name, in the repository's build and in a release.
    pub fn file_name(self) -> &'static str {
        match self {
            Plugin::Convert => "film_lab_convert.wasm",
            Plugin::Advanced => "film_lab_convert_advanced.wasm",
        }
    }
}

/// How the whole-image analysis treats the scan.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzeOptions {
    /// Percentage of each edge left out (film holder, sprocket holes, the border itself).
    pub border_percent: f32,
    /// Share of the brightest samples that may lie above the film base (dust, hot pixels), %.
    pub base_clip: f32,
    /// Share of samples allowed below black and above white, %.
    pub black_clip: f32,
    pub white_clip: f32,
    pub wb: WbMode,
    /// At most this many pixels are sampled (a regular grid over the image).
    pub max_samples: usize,
}

impl Default for AnalyzeOptions {
    /// [`MeasureOptions::DEFAULT`], which `pc_analyze` uses too.
    fn default() -> Self {
        let m = MeasureOptions::DEFAULT;
        Self {
            border_percent: m.border_percent,
            base_clip: m.base_clip,
            black_clip: m.black_clip,
            white_clip: m.white_clip,
            wb: if m.neutral { WbMode::Neutral } else { WbMode::Auto },
            max_samples: m.max_samples,
        }
    }
}

impl AnalyzeOptions {
    /// The measurement these options ask for; the white-balance offset of Warm and Cool goes on
    /// top ([`Levels::plugin_params`]).
    pub fn measure_options(&self) -> MeasureOptions {
        MeasureOptions {
            border_percent: self.border_percent,
            base_clip: self.base_clip,
            black_clip: self.black_clip,
            white_clip: self.white_clip,
            neutral: self.wb == WbMode::Neutral,
            max_samples: self.max_samples,
        }
    }
}

/// The automatic white balance of an analysis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WbMode {
    /// Levels per channel: the black and white points are neutral, the scene's own colour
    /// stays (a green garden stays green), but so does any cast in the mid-tones: on the Ektar
    /// 100 sample they lean blue.
    Auto,
    /// Auto, then each channel's median brought to the common median with a per-channel gamma:
    /// a grey scene average, which over-corrects scenes dominated by one colour. The default:
    /// the closest to the reference positives.
    #[default]
    Neutral,
    /// Auto, then a touch warmer.
    Warm,
    /// Auto, then a touch cooler.
    Cool,
}

impl WbMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "neutral" => Some(Self::Neutral),
            "warm" => Some(Self::Warm),
            "cool" => Some(Self::Cool),
            _ => None,
        }
    }

    fn temp(self) -> f32 {
        match self {
            Self::Warm => 8.0,
            Self::Cool => -8.0,
            Self::Auto | Self::Neutral => 0.0,
        }
    }
}

/// What the analysis found, as scan values on the 0..255 scale the plug-in's parameters use.
#[derive(Clone, Debug, PartialEq)]
pub struct Levels {
    pub base: [f32; 3],
    pub black: [f32; 3],
    pub white: [f32; 3],
    pub temp: f32,
    /// Per-channel mid-tone gammas that neutralise the scene average (1 = none).
    pub balance: [f32; 3],
    /// Pixels the analysis looked at.
    pub samples: u32,
}

impl Levels {
    /// The measurement as the JSON the plug-in's block carries: film base, black and white
    /// points (scan values, 0..255) and the balance.
    pub fn measurement(&self) -> Map<String, Value> {
        let mut m = Map::new();
        for (prefix, v) in [("base", self.base), ("black", self.black), ("white", self.white)] {
            for (suffix, x) in ["_r", "_g", "_b"].iter().zip(v) {
                m.insert(format!("{prefix}{suffix}"), json!(round1(x)));
            }
        }
        for (suffix, g) in ["_r", "_g", "_b"].iter().zip(self.balance) {
            m.insert(format!("balance{suffix}"), json!((f64::from(g) * 1000.0).round() / 1000.0));
        }
        m
    }

    /// The plug-in parameters that go with the measurement (merge the user's preset and
    /// sliders on top): the white-balance offset of the mode chosen. The measurement itself
    /// travels inside the module.
    pub fn plugin_params(&self) -> Map<String, Value> {
        let mut m = Map::new();
        if self.temp != 0.0 {
            m.insert("temp".into(), json!(self.temp));
        }
        m
    }

    /// `wasm` (the plug-in) with this measurement written into its block, as the module to
    /// install: the dialog never shows the numbers, and every band reads the same ones.
    pub fn embed(&self, wasm: &[u8]) -> Result<Vec<u8>, String> {
        embed_measurement(wasm, &Value::Object(self.measurement()).to_string())
    }
}

/// The marker that starts the plug-in's measurement block, and the block's size
/// (`crates/photocraft`).
pub const MEASUREMENT_MARKER: &[u8] = b"FILMLAB-MEASURE-1:";
pub const MEASUREMENT_LEN: usize = 512;

/// `wasm` with `json` written into its measurement block, padded with spaces.
pub fn embed_measurement(wasm: &[u8], json: &str) -> Result<Vec<u8>, String> {
    let at = wasm
        .windows(MEASUREMENT_MARKER.len())
        .position(|w| w == MEASUREMENT_MARKER)
        .ok_or("this module has no measurement block (not the Film Lab plug-in?)")?;
    let room = MEASUREMENT_LEN - MEASUREMENT_MARKER.len();
    if json.len() > room {
        return Err(format!("the measurement is {} bytes, the block holds {room}", json.len()));
    }
    let mut out = wasm.to_vec();
    let start = at + MEASUREMENT_MARKER.len();
    let slot = out.get_mut(start..start + room).ok_or("the module ends inside its measurement block")?;
    slot.fill(b' ');
    slot.get_mut(..json.len()).ok_or("no room")?.copy_from_slice(json.as_bytes());
    Ok(out)
}

fn round1(x: f32) -> f64 {
    (f64::from(x) * 10.0).round() / 10.0
}

/// Analyses `img` as a negative (any colour type; 16-bit stays 16-bit until it is normalised),
/// with [`film_lab_core::measure`]: the same measurement the plug-ins' `pc_analyze` makes.
pub fn analyze(img: &DynamicImage, o: &AnalyzeOptions) -> Result<Levels, String> {
    let rgb = img.to_rgb32f();
    let mut hist = ValueHistogram::new();
    let m = film_lab_core::measure(rgb.width() as usize, rgb.height() as usize, &o.measure_options(), &mut hist, |x, y| {
        let p = rgb.get_pixel_checked(u32::try_from(x).ok()?, u32::try_from(y).ok()?)?.0;
        Some([p[0], p[1], p[2]])
    })
    .map_err(|e| e.to_string())?;
    Ok(Levels { base: m.base, black: m.black, white: m.white, temp: o.wb.temp(), balance: m.balance, samples: m.samples })
}

/// The average colour of the `size × size` square centred on (`x`, `y`), on the 0..255 scale the
/// plug-in's colour parameters use: what the Eyedropper and the Info panel show in the app.
pub fn sample(img: &DynamicImage, x: u32, y: u32, size: u32) -> [f32; 3] {
    let rgb = img.to_rgb32f();
    let half = size.max(1) / 2;
    let (x0, x1) = (x.saturating_sub(half), (x + half + 1).min(rgb.width()));
    let (y0, y1) = (y.saturating_sub(half), (y + half + 1).min(rgb.height()));
    let mut sum = [0.0f64; 3];
    let mut n = 0u32;
    for yy in y0..y1 {
        for xx in x0..x1 {
            let p = rgb.get_pixel(xx, yy).0;
            for ch in 0..3 {
                sum[ch] += f64::from(p[ch]);
            }
            n += 1;
        }
    }
    let n = f64::from(n.max(1));
    [(sum[0] / n * 255.0) as f32, (sum[1] / n * 255.0) as f32, (sum[2] / n * 255.0) as f32]
}

/// Where `plugin`'s `.wasm` is: `--plugin`, `FILM_LAB_WASM`, or the repository's build.
pub fn find_plugin(explicit: Option<&Path>, plugin: Plugin) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("FILM_LAB_WASM") {
        return Ok(PathBuf::from(p));
    }
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../photocraft/target/wasm32-unknown-unknown/release").join(plugin.file_name());
    if built.is_file() {
        return Ok(built);
    }
    Err("no plug-in module: pass --plugin PATH, set FILM_LAB_WASM, or run crates/photocraft/build.sh".into())
}

/// Where `photocraft-cli` is: `--photocraft-cli`, `PHOTOCRAFT_CLI`, or the PATH.
pub fn find_photocraft_cli(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("PHOTOCRAFT_CLI").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("photocraft-cli"))
}

/// Writes `wasm` with `levels` embedded to a temporary file for `photocraft-cli` to install.
fn embedded_module(wasm: &Path, levels: &Levels) -> Result<PathBuf, String> {
    let bytes = std::fs::read(wasm).map_err(|e| format!("cannot read the plug-in {}: {e}", wasm.display()))?;
    let patched = levels.embed(&bytes)?;
    let path = std::env::temp_dir().join(format!("film-lab-{}.wasm", std::process::id()));
    std::fs::write(&path, patched).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Runs `plugin` (the module `wasm`) with `levels` embedded on `input` through `photocraft-cli
/// run`, writing `output`; `params` are the preset and sliders on top.
pub fn convert_one(
    cli: &Path,
    plugin: Plugin,
    wasm: &Path,
    levels: &Levels,
    input: &Path,
    output: &Path,
    params: &Map<String, Value>,
) -> Result<(), String> {
    let module = embedded_module(wasm, levels)?;
    let install = json!({"path": module}).to_string();
    let run = json!({"id": plugin.id(), "params": params}).to_string();
    let status = Command::new(cli)
        .arg("run")
        .arg(input)
        .args(["--cmd", "plugin.install", "--params", &install, "--cmd", "plugin.run", "--params", &run, "--out"])
        .arg(output)
        // photocraft-cli prints each command's result (the whole manifest); errors still reach stderr.
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {}: {e}", cli.display()));
    std::fs::remove_file(&module).ok();
    if status?.success() { Ok(()) } else { Err("photocraft-cli failed".into()) }
}

/// Runs `plugin` (the module `wasm`) with `levels` embedded on every image in `in_dir` through
/// `photocraft-cli batch` (one measurement for the roll), writing `out_dir`; `format` is an
/// extension or None.
#[allow(clippy::too_many_arguments)]
pub fn convert_roll(
    cli: &Path,
    plugin: Plugin,
    wasm: &Path,
    levels: &Levels,
    in_dir: &Path,
    out_dir: &Path,
    params: &Map<String, Value>,
    format: Option<&str>,
) -> Result<(), String> {
    let module = embedded_module(wasm, levels)?;
    let actions = json!([
        {"command": "plugin.install", "params": {"path": module}},
        {"command": "plugin.run", "params": {"id": plugin.id(), "params": params}},
    ]);
    let actions_path = std::env::temp_dir().join(format!("film-lab-actions-{}.json", std::process::id()));
    std::fs::write(&actions_path, actions.to_string()).map_err(|e| format!("cannot write {}: {e}", actions_path.display()))?;
    let mut cmd = Command::new(cli);
    cmd.arg("batch").arg("--actions").arg(&actions_path).arg("--in").arg(in_dir).arg("--out").arg(out_dir);
    if let Some(f) = format {
        cmd.args(["--format", f]);
    }
    let status = cmd.stdout(Stdio::null()).status().map_err(|e| format!("cannot run {}: {e}", cli.display()));
    std::fs::remove_file(&actions_path).ok();
    std::fs::remove_file(&module).ok();
    if status?.success() { Ok(()) } else { Err("photocraft-cli batch failed".into()) }
}
