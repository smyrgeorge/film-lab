//! `pc_analyze` (ABI v1.1 proposal): the whole-frame measurement a host passes back as
//! `_analysis` gives the same conversion as the companion's measurement in the module, in linear
//! and sRGB alike; without `_analysis` nothing changes.

mod common;

use common::*;
use film_lab_core::color;
use film_lab_photocraft_harness::{Host, MODE_GRAY, MODE_RGB, Plugin, embed_measurement, format_code};

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

/// `pc_analyze`'s result as the companion writes it into the module's block.
fn as_block(a: &serde_json::Value) -> String {
    let mut m = serde_json::Map::new();
    for key in ["base", "black", "white", "balance"] {
        for (ch, suffix) in ["_r", "_g", "_b"].iter().enumerate() {
            m.insert(format!("{key}{suffix}"), a[key][ch].clone());
        }
    }
    serde_json::Value::Object(m).to_string()
}

/// A colour measurement: the frame's, with its channels pulled apart so a grey document's choice
/// of channel shows.
fn colour_analysis(h: &mut Host) -> serde_json::Value {
    let mut a: serde_json::Value =
        serde_json::from_str(&h.analyze(&frame(false), 64, 64, 3, format_code(8, false, MODE_RGB), "{}").unwrap().unwrap()).unwrap();
    a["balance"] = serde_json::json!([0.8, 1.0, 1.3]);
    a
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
        let mut embedded = Host::load(&embed_measurement(&wasm(plugin), &as_block(&b)).unwrap()).unwrap();
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
fn a_grey_document_reads_both_measurements_alike() {
    for plugin in Plugin::ALL {
        let mut plain = host(plugin);
        let a = colour_analysis(&mut plain);
        let mut embedded = Host::load(&embed_measurement(&wasm(plugin), &as_block(&a)).unwrap()).unwrap();
        let with = format!(r#"{{"_analysis": {a}}}"#);
        for s in [0.1, 0.4, 0.8] {
            let grey = |h: &mut Host, params: &str| {
                let mut px = [negative(s, 1)];
                assert_eq!(h.filter(&mut px, 1, 1, 1, format_code(8, false, MODE_GRAY), params).unwrap(), 0);
                px[0]
            };
            let (via_block, via_param) = (grey(&mut embedded, "{}"), grey(&mut plain, &with));
            assert!(close(via_block, via_param, 1e-5), "{plugin:?} at {s}: block {via_block} vs _analysis {via_param}");
        }
    }
}

#[test]
fn analysis_is_read_from_any_well_formed_json() {
    let mut h = host(Plugin::Convert);
    let a = colour_analysis(&mut h);
    let reference = one(&mut h, 0.3, &format!(r#"{{"_analysis": {a}}}"#));
    let (base, black, white, balance) = (&a["base"], &a["black"], &a["white"], &a["balance"]);
    // Members in another order, extra members of any kind (escaped quotes, braces and brackets in
    // strings, nested objects and arrays), whitespace, and other parameters around it.
    let rich = format!(
        r#"{{ "note": "a \"quoted\" {{brace}} and ] bracket", "_image": {{"x": 0, "_analysis": {{"filmlab": 2}}}},
            "_analysis" : {{ "samples": 9, "white" : {white}, "extra": {{"base": [0, 0, 0], "list": [[1], {{"a": "]"}}]}},
              "balance": {balance}, "scale": "srgb255", "black": {black}, "base": {base}, "filmlab": 1 }}, "exposure": 0 }}"#
    );
    assert_eq!(one(&mut h, 0.3, &rich), reference, "{rich}");

    // Anything else falls back to running without a measurement, never traps.
    let fallback = one(&mut h, 0.3, "{}");
    for bad in [
        format!(r#"{{"_analysis": {{"filmlab": 2, "scale": "srgb255", "base": {base}, "black": {black}, "white": {white}}}}}"#),
        format!(r#"{{"_analysis": {{"filmlab": 1, "scale": "linear", "base": {base}, "black": {black}, "white": {white}}}}}"#),
        format!(r#"{{"_analysis": {{"filmlab": 1, "scale": "srgb255", "base": {base}, "black": {black}}}}}"#),
        format!(r#"{{"_analysis": {{"filmlab": 1, "scale": "srgb255", "base": [1, 2], "black": {black}, "white": {white}}}}}"#),
        format!(r#"{{"_analysis": {{"filmlab": 1, "scale": "srgb255", "base": [1, 2, "x"], "black": {black}, "white": {white}}}}}"#),
        format!(r#"{{"note": ""_analysis": {a}"}}"#),
        format!(r#"{{"nested": {{"_analysis": {a}}}}}"#),
        format!(r#"{{"_analysis": "{{}}", "x": {a}}}"#),
        r#"{"_analysis": {"filmlab": 1, "base": [1, 2}}"#.to_string(),
        r#"{"_analysis": {"filmlab": 1, "scale": "srgb255", "base": [1e999, 0, 0]"#.to_string(),
        r#"{"_analysis": "#.to_string(),
        format!(r#"{{"_analysis": {}}}"#, "[".repeat(10_000)),
        format!(r#"{{"s": "{}", "_analysis": {a}}}"#, "\\".repeat(3)),
    ] {
        assert_eq!(one(&mut h, 0.3, &bad), fallback, "{bad}");
    }
}

#[test]
fn manifests_carry_the_stage_hint() {
    for plugin in Plugin::ALL {
        let m: serde_json::Value = serde_json::from_str(&host(plugin).manifest().unwrap()).unwrap();
        assert_eq!(m["stage"], "source");
    }
}
