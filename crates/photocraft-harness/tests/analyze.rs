//! `pc_analyze` (ABI v1.1 proposal): the whole-frame measurement a host passes back as
//! `_analysis` gives the same conversion as the companion's measurement in the module, in linear
//! and sRGB alike; without `_analysis` nothing changes.

mod common;

use common::*;
use film_lab_core::color;
use film_lab_photocraft_harness::{MODE_RGB, Plugin, embed_measurement, format_code};

/// A 64 × 64 RGB frame: a log ramp of scene values over the inner area, the film base on the
/// border. `linear`: linear samples, else sRGB-encoded.
fn frame(linear: bool) -> Vec<f32> {
    let mut px = Vec::new();
    for y in 0..64 {
        for x in 0..64 {
            let s = ((y * 64 + x) as f32 / 4095.0).clamp(0.0, 1.0);
            for ch in 0..3 {
                let v = negative_linear(s, ch);
                px.push(if linear { v } else { color::srgb_encode(v) });
            }
        }
    }
    px
}

#[test]
fn analysis_round_trips_through_filter_params() {
    for plugin in Plugin::ALL {
        let mut h = host(plugin);
        let lin = h.analyze(&frame(true), 64, 64, 3, format_code(32, false, MODE_RGB), r#"{"encoding": "linear"}"#).unwrap().unwrap();
        let srgb = h.analyze(&frame(false), 64, 64, 3, format_code(8, false, MODE_RGB), "{}").unwrap().unwrap();
        let (a, b): (serde_json::Value, serde_json::Value) = (serde_json::from_str(&lin).unwrap(), serde_json::from_str(&srgb).unwrap());
        for key in ["base", "black", "white"] {
            for ch in 0..3 {
                let (x, y) = (a[key][ch].as_f64().unwrap(), b[key][ch].as_f64().unwrap());
                assert!((x - y).abs() < 1.0, "{plugin:?}: {key} differs between encodings: {lin} vs {srgb}");
            }
        }
        // White is darker than black and both under the base, per channel, in a negative.
        for ch in 0..3 {
            assert!(a["white"][ch].as_f64() < a["black"][ch].as_f64() && a["black"][ch].as_f64() <= a["base"][ch].as_f64(), "{lin}");
        }

        // The same numbers in the module's block and in `_analysis` convert alike.
        let block = format!(
            r#"{{"base_r": {}, "base_g": {}, "base_b": {}, "black_r": {}, "black_g": {}, "black_b": {}, "white_r": {}, "white_g": {}, "white_b": {}}}"#,
            b["base"][0], b["base"][1], b["base"][2], b["black"][0], b["black"][1], b["black"][2], b["white"][0], b["white"][1], b["white"][2]
        );
        let mut embedded = film_lab_photocraft_harness::Host::load(&embed_measurement(&wasm(plugin), &block).unwrap()).unwrap();
        let mut plain = host(plugin);
        let with = format!(r#"{{"_analysis": {srgb}}}"#);
        for s in [0.1, 0.5, 0.9] {
            let via_block = one(&mut embedded, s, "{}");
            let via_param = one(&mut plain, s, &with);
            let none = one(&mut plain, s, "{}");
            for ch in 0..3 {
                assert!(close(via_block[ch], via_param[ch], 1e-4), "{plugin:?} at {s}: {via_block:?} vs {via_param:?}");
            }
            assert!(via_param != none || s == 0.5, "{plugin:?}: `_analysis` changed nothing at {s}");
        }
    }
}

#[test]
fn flat_or_tiny_frames_measure_nothing() {
    let mut h = host(Plugin::Convert);
    assert_eq!(h.analyze(&[0.3; 3 * 16], 4, 4, 3, format_code(32, false, MODE_RGB), "{}").unwrap(), None);
    assert_eq!(h.analyze(&[0.3; 3], 1, 1, 3, format_code(32, false, MODE_RGB), "{}").unwrap(), None);
    // A malformed `_analysis` falls back to the default base instead of failing.
    let a = one(&mut h, 0.5, r#"{"_analysis": {"filmlab": 1, "base": [1, 2}}"#);
    let b = one(&mut h, 0.5, "{}");
    assert_eq!(a, b);
}

#[test]
fn manifests_carry_the_stage_hint() {
    for plugin in Plugin::ALL {
        let m: serde_json::Value = serde_json::from_str(&host(plugin).manifest().unwrap()).unwrap();
        assert_eq!(m["stage"], "source");
    }
}
