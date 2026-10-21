//! Film Lab: Convert Negative, a PhotoCraft filter plug-in (ABI v1, through
//! [`film_lab_photocraft`]).
//!
//! The host hands over one band of the layer as interleaved `f32` samples in the document's
//! colour model; this module converts the negative in place with [`film_lab_core`], leaving
//! alpha and fully transparent pixels alone.
//!
//! # The dialog
//!
//! A preset (the character of a film stock: layer contrast, mask colour, tone profile, colour
//! model, saturation, warmth, toning) and six sliders on top of it: exposure, brightness,
//! contrast, temperature, tint, saturation. Everything else is internal; the advanced plug-in
//! (`crates/photocraft/advanced`) has the rest.
//!
//! # The film base
//!
//! The companion's measurement in the module ([`film_lab_photocraft::Band::measurement`]) when
//! there is one. Without one, the preset's typical mask colour stands in for the film base, with a
//! fixed range per channel ([`film_lab_core::Preset::scan_gamma`]): identical on every band, close
//! when the scanning setup matches the preset.
//!
//! A host may also pass `base_r/g/b` (0..255, the Info panel's values) in the JSON: they replace
//! the preset's base when the module holds no measurement. Other parameters of the core are
//! honoured from the JSON too (`range`, `neutral_*`, `encoding`) but have no dialog control.
#![no_std]

use film_lab_core::{Preset, Sliders, params};
use film_lab_photocraft::{Band, OK, plugin};

/// The manifest PhotoCraft reads at install (`pc_manifest`); parameters in dialog order. No
/// parameter is named like a key of the host's `_image` object (see `film_lab_core::params`). The
/// version is the workspace's, so a release tag only has to match `Cargo.toml`. The name has no
/// "…": PhotoCraft adds one to the menu entry of a plug-in with parameters, and shows the name
/// as it is in the dialog's title and the History panel.
const MANIFEST: &str = concat!(
    r#"{
  "id": "io.github.smyrgeorge.film-lab.convert",
  "name": "Film Lab: Convert Negative",
  "version": ""#,
    env!("CARGO_PKG_VERSION"),
    r#"",
  "kind": "filter",
  "author": "film-lab",
  "description": "Converts a scanned film negative into a positive: pick a film preset, adjust exposure, brightness, contrast, temperature, tint and saturation. The companion measures the whole image for it.",
  "params": {
    "preset":     {"type": "choice", "options": ["Standard", "Portra 160", "Portra 400", "Ektar 100", "Gold 200", "ColorPlus 200", "UltraMax 400", "Superia 400", "C200", "Pro 400H", "CineStill 800", "Monochrome"], "default": "Standard"},
    "exposure":   {"type": "number", "min": -5, "max": 5, "default": 0},
    "brightness": {"type": "number", "min": -50, "max": 50, "default": 0},
    "contrast":   {"type": "number", "min": -50, "max": 50, "default": 0},
    "temp":       {"type": "number", "min": -50, "max": 50, "default": 0},
    "tint":       {"type": "number", "min": -50, "max": 50, "default": 0},
    "saturation": {"type": "number", "min": -50, "max": 50, "default": 0}
  },
  "overlap": 0,
  "area": "content",
  "stage": "source"
}"#
);

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
    if let Some(range) = params::number(json, "range") {
        p.range = range;
    }

    // The base and levels: the companion's measurement in the module, or the preset's mask (or a
    // typed base) with the preset's range, the same on every band. Measuring the band itself
    // would give every band its own levels.
    let measured = band.measurement().and_then(|m| Some((m.analysis(band.enc)?, m.balance)));
    let analysis = match measured {
        Some((a, balance)) => {
            p.balance = balance;
            a
        }
        None => {
            let preset_base = if band.grey() { [preset.base[1]; 3] } else { preset.base };
            band.from_base(band.colour_param(["base_r", "base_g", "base_b"]).unwrap_or(preset_base), p.film_gamma)
        }
    };
    let neutral = band.colour_param(["neutral_r", "neutral_g", "neutral_b"]);
    band.convert(&analysis, &p, neutral);
    OK
}
