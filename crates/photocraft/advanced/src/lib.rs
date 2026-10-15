//! Film Lab: Convert Negative (Advanced), a PhotoCraft filter plug-in (ABI v1, through
//! [`film_lab_photocraft`]).
//!
//! The conversion of Convert Negative (`crates/photocraft/convert`) with the controls it keeps
//! internal, in one flat dialog (PhotoCraft draws a manifest without sections):
//!
//! - **The film base, picked from the scan**: Film Base Red / Green / Blue, the border's colour
//!   as the Info panel shows it (0..255; sample the unexposed border with the Eyedropper). Set, it
//!   is the film base, over the companion's measured one and the preset's: with a measurement in
//!   the module its white point and balance still apply. All 0 (the default) leaves the base to
//!   the measurement, or to the preset's mask colour without one.
//! - Exposure, brightness and contrast; the tone curve (or the preset's), highlights / shadows,
//!   whites / blacks, fade / glow, and the dynamic range: stops between black and white.
//! - White balance (a preset, temperature, tint) and a gray point, a scan colour that should
//!   come out grey (0 for none); the colour look (or the preset's) and saturation.
//! - Output sharpening, which is why the manifest asks for one pixel of overlap.
//!
//! # Labels
//!
//! PhotoCraft labels a control from its key, splitting camelCase into words (`filmBaseRed` is
//! Film Base Red), and shows an option with a space exactly as written; so keys are camelCase and
//! options are the menu's text, mapped to the core's values by the tables below. A key named
//! `amount` gets a % unit.
#![no_std]

use film_lab_core::{Look, Preset, Sharpen, Sliders, Tone, WbPreset, params};
use film_lab_photocraft::{Band, Measurement, OK, plugin};

/// The manifest PhotoCraft reads at install (`pc_manifest`); parameters in dialog order. No
/// parameter is named like a key of the host's `_image` object (see `film_lab_core::params`).
const MANIFEST: &str = concat!(
    r#"{
  "id": "io.github.smyrgeorge.film-lab.convert-advanced",
  "name": "Film Lab: Convert Negative (Advanced)",
  "version": ""#,
    env!("CARGO_PKG_VERSION"),
    r#"",
  "kind": "filter",
  "author": "film-lab",
  "description": "Converts a scanned film negative into a positive with every control: the film base picked from the border (0 = measured or the preset's), tone, white balance, a gray point, colour and sharpening.",
  "params": {
    "preset":         {"type": "choice", "options": ["Standard", "Portra 160", "Portra 400", "Ektar 100", "Gold 200", "ColorPlus 200", "UltraMax 400", "Superia 400", "C200", "Pro 400H", "CineStill 800", "Monochrome"], "default": "Standard"},
    "filmBaseRed":    {"type": "int", "min": 0, "max": 255, "default": 0},
    "filmBaseGreen":  {"type": "int", "min": 0, "max": 255, "default": 0},
    "filmBaseBlue":   {"type": "int", "min": 0, "max": 255, "default": 0},
    "exposure":       {"type": "number", "min": -5, "max": 5, "default": 0},
    "brightness":     {"type": "number", "min": -50, "max": 50, "default": 0},
    "contrast":       {"type": "number", "min": -50, "max": 50, "default": 0},
    "toneCurve":      {"type": "choice", "options": ["From Preset", "Lab Standard", "Lab Soft", "Lab Hard", "Lab Highlight Soft", "Lab Highlight Hard", "Lab Shadow Soft", "Lab Shadow Hard", "Linear", "Linear Deep", "Linear Flat", "Cinematic", "Cinematic Flat", "Cinematic Rich"], "default": "From Preset"},
    "highlights":     {"type": "number", "min": -50, "max": 50, "default": 0},
    "shadows":        {"type": "number", "min": -50, "max": 50, "default": 0},
    "whites":         {"type": "number", "min": -50, "max": 50, "default": 0},
    "blacks":         {"type": "number", "min": -50, "max": 50, "default": 0},
    "fade":           {"type": "number", "min": -100, "max": 100, "default": 0},
    "glow":           {"type": "number", "min": -100, "max": 100, "default": 0},
    "dynamicRange":   {"type": "number", "min": 1, "max": 10, "default": 3},
    "whiteBalance":   {"type": "choice", "options": ["As Shot", "Warm", "Cool", "Tungsten Film"], "default": "As Shot"},
    "temp":           {"type": "number", "min": -50, "max": 50, "default": 0},
    "tint":           {"type": "number", "min": -50, "max": 50, "default": 0},
    "grayPointRed":   {"type": "int", "min": 0, "max": 255, "default": 0},
    "grayPointGreen": {"type": "int", "min": 0, "max": 255, "default": 0},
    "grayPointBlue":  {"type": "int", "min": 0, "max": 255, "default": 0},
    "colorLook":      {"type": "choice", "options": ["From Preset", "Neutral", "Lab Warm", "Lab Cool", "Print", "Clean"], "default": "From Preset"},
    "saturation":     {"type": "number", "min": -50, "max": 50, "default": 0},
    "sharpening":     {"type": "choice", "options": ["Off", "Lab", "Scanner"], "default": "Off"},
    "amount":         {"type": "number", "min": 0, "max": 100, "default": 50}
  },
  "overlap": 1,
  "area": "content",
  "stage": "source"
}"#
);

/// `toneCurve`'s options; "From Preset", listed first in the manifest, is the preset's own.
const TONE_CURVES: [(&str, Tone); 13] = [
    ("Lab Standard", Tone::LabStandard),
    ("Lab Soft", Tone::LabSoft),
    ("Lab Hard", Tone::LabHard),
    ("Lab Highlight Soft", Tone::LabHighlightSoft),
    ("Lab Highlight Hard", Tone::LabHighlightHard),
    ("Lab Shadow Soft", Tone::LabShadowSoft),
    ("Lab Shadow Hard", Tone::LabShadowHard),
    ("Linear", Tone::Linear),
    ("Linear Deep", Tone::LinearDeep),
    ("Linear Flat", Tone::LinearFlat),
    ("Cinematic", Tone::Cinematic),
    ("Cinematic Flat", Tone::CinematicFlat),
    ("Cinematic Rich", Tone::CinematicRich),
];

const WHITE_BALANCES: [(&str, WbPreset); 4] =
    [("As Shot", WbPreset::AsIs), ("Warm", WbPreset::Warm), ("Cool", WbPreset::Cool), ("Tungsten Film", WbPreset::TungstenFilm)];

/// `colorLook`'s options; "From Preset" is the preset's own.
const COLOR_LOOKS: [(&str, Look); 5] =
    [("Neutral", Look::Neutral), ("Lab Warm", Look::LabWarm), ("Lab Cool", Look::LabCool), ("Print", Look::Print), ("Clean", Look::Clean)];

const SHARPENING: [(&str, Sharpen); 3] = [("Off", Sharpen::Off), ("Lab", Sharpen::Lab), ("Scanner", Sharpen::Scanner)];

plugin!(MANIFEST, filter);

fn filter(mut band: Band) -> i32 {
    let json = band.json;
    let preset = params::string(json, "preset").and_then(Preset::parse).unwrap_or_else(Preset::default_preset);
    let num = |key: &str| params::number(json, key).unwrap_or(0.0);
    let sliders = Sliders {
        exposure: num("exposure"),
        brightness: num("brightness"),
        contrast: num("contrast"),
        temp: num("temp"),
        tint: num("tint"),
        saturation: num("saturation"),
    };
    let mut p = preset.params(&sliders);
    // "From Preset" (or anything unlisted) is in no table, and keeps the preset's own.
    if let Some(tone) = band.choice("toneCurve", &TONE_CURVES) {
        p.tone = tone;
    }
    if let Some(look) = band.choice("colorLook", &COLOR_LOOKS) {
        p.look = look;
    }
    if let Some(wb) = band.choice("whiteBalance", &WHITE_BALANCES) {
        p.wb = wb;
    }
    p.lights = num("highlights");
    p.darks = num("shadows");
    p.whites = num("whites");
    p.blacks = num("blacks");
    p.fade = num("fade");
    p.glow = num("glow");
    if let Some(range) = params::number(json, "dynamicRange") {
        p.range = range;
    }
    p.sharpen = band.choice("sharpening", &SHARPENING).unwrap_or(p.sharpen);
    p.sharpen_amount = params::number(json, "amount").unwrap_or(p.sharpen_amount);

    // The base: picked in the dialog, else the companion's measurement, else the preset's mask.
    // A picked base keeps the measurement's white point and balance: the base is black.
    let picked = band.colour_param(["filmBaseRed", "filmBaseGreen", "filmBaseBlue"]);
    let measured = band.measurement().and_then(|m| {
        let m = match picked {
            Some(base) => {
                let base = m.reencode(band.enc, base);
                Measurement { base, black: base, ..m }
            }
            None => m,
        };
        Some((m.analysis(band.enc)?, m.balance))
    });
    let analysis = match measured {
        Some((a, balance)) => {
            p.balance = balance;
            a
        }
        None => {
            let preset_base = if band.grey() { [preset.base[1]; 3] } else { preset.base };
            band.from_base(picked.unwrap_or(preset_base), p.film_gamma)
        }
    };
    let neutral = band.colour_param(["grayPointRed", "grayPointGreen", "grayPointBlue"]);
    band.convert(&analysis, &p, neutral);
    OK
}
