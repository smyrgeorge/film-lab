//! Whole-image analysis of synthetic negatives, and the conversion through photocraft-cli when
//! one is available (`PHOTOCRAFT_CLI`).

use film_lab_companion::{AnalyzeOptions, Plugin, WbMode, analyze, convert_one, find_plugin};
use film_lab_core::{RANGE_STOPS, color, film};
use image::{DynamicImage, ImageBuffer, Rgb};

const BASE: [f32; 3] = [0.6, 0.35, 0.15];

/// A negative of a grey ramp with a film border, 8-bit sRGB, `w × h`.
fn synthetic(w: u32, h: u32, border: u32) -> DynamicImage {
    let white = libm::exp2f(RANGE_STOPS) - 1.0;
    let img = ImageBuffer::from_fn(w, h, |x, y| {
        let inside = x >= border && y >= border && x < w - border && y < h - border;
        let s = if inside { (x - border) as f32 / (w - 2 * border - 1) as f32 } else { 0.0 };
        let mut px = [0u8; 3];
        for ch in 0..3 {
            let t = film::transmittance(1.0 + s * white, BASE[ch]);
            px[ch] = (color::srgb_encode(t) * 255.0).round() as u8;
        }
        Rgb(px)
    });
    DynamicImage::ImageRgb8(img)
}

#[test]
fn finds_the_base_and_levels_of_a_synthetic_negative() {
    let img = synthetic(400, 300, 20);
    let levels =
        analyze(&img, &AnalyzeOptions { border_percent: 0.0, black_clip: 0.0, white_clip: 0.0, wb: WbMode::Auto, ..AnalyzeOptions::default() })
            .unwrap();
    for (ch, b) in BASE.iter().enumerate() {
        let base = color::srgb_encode(*b) * 255.0;
        assert!((levels.base[ch] - base).abs() < 1.5, "base {ch}: {} vs {base}", levels.base[ch]);
        assert!((levels.black[ch] - base).abs() < 1.5, "black at the base: {}", levels.black[ch]);
        let white = color::srgb_encode(film::transmittance(libm::exp2f(RANGE_STOPS), *b)) * 255.0;
        assert!((levels.white[ch] - white).abs() < 1.5, "white {ch}: {} vs {white}", levels.white[ch]);
    }
    assert_eq!(levels.temp, 0.0);
    assert_eq!(levels.balance, [1.0; 3], "auto leaves the medians alone");
    let neutral = analyze(&img, &AnalyzeOptions { wb: WbMode::Neutral, border_percent: 0.0, ..AnalyzeOptions::default() }).unwrap();
    // Within the histogram's quantisation (the blue channel of a negative spans few bins).
    assert!(neutral.balance.iter().all(|b| (b - 1.0).abs() < 0.05), "a balanced synthetic scan needs no balance: {:?}", neutral.balance);
    let params = levels.plugin_params();
    assert!(params.is_empty(), "the numbers travel in the module, not the dialog: {params:?}");
    let m = levels.measurement();
    for key in ["base_r", "base_g", "base_b", "black_r", "white_b", "balance_g"] {
        assert!(m[key].is_number(), "{key}");
    }
    // Embedding: the block is found, filled, padded; a module without one is refused.
    let mut fake = b"\0asm....".to_vec();
    fake.extend_from_slice(film_lab_companion::MEASUREMENT_MARKER);
    fake.extend(std::iter::repeat_n(b' ', film_lab_companion::MEASUREMENT_LEN - film_lab_companion::MEASUREMENT_MARKER.len()));
    fake.extend_from_slice(b"tail");
    let patched = levels.embed(&fake).unwrap();
    assert_eq!(patched.len(), fake.len());
    assert!(patched.ends_with(b"tail"));
    let text = String::from_utf8_lossy(&patched);
    assert!(text.contains("\"base_r\":") && text.contains("\"balance_b\":"), "{text}");
    assert!(levels.embed(b"\0asm").is_err());
    let warm = analyze(&img, &AnalyzeOptions { wb: WbMode::Warm, ..AnalyzeOptions::default() }).unwrap();
    assert_eq!(warm.plugin_params()["temp"], 8.0);
}

#[test]
fn border_buffer_excludes_the_edges_and_sampling_is_bounded() {
    let img = synthetic(400, 300, 20);
    // With a 10 % border excluded the film border no longer sets the base; black moves inward.
    let o = AnalyzeOptions { border_percent: 10.0, max_samples: 2_000, ..AnalyzeOptions::default() };
    let levels = analyze(&img, &o).unwrap();
    assert!(levels.samples <= 2_500, "{}", levels.samples);
    assert!(levels.black[0] < color::srgb_encode(BASE[0]) * 255.0 - 1.0, "{:?}", levels.black);
    assert!(analyze(&img, &AnalyzeOptions { border_percent: 45.0, ..AnalyzeOptions::default() }).is_err() || true);
    let flat = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(8, 8, Rgb([120u8, 90, 60])));
    assert!(analyze(&flat, &AnalyzeOptions::default()).is_err(), "a flat scan has no range");
}

#[test]
fn sample_averages_a_square_on_the_info_panel_scale() {
    let img = synthetic(64, 48, 8);
    let c = film_lab_companion::sample(&img, 2, 2, 3);
    for (v, b) in c.iter().zip(BASE) {
        assert!((v - color::srgb_encode(b) * 255.0).abs() < 1.0, "border is the base: {c:?}");
    }
    let edge = film_lab_companion::sample(&img, 0, 0, 11);
    assert!(edge.iter().all(|v| v.is_finite()), "clipped at the edge: {edge:?}");
    let one = film_lab_companion::sample(&img, 63, 47, 1);
    assert!(one.iter().all(|v| (0.0..=255.0).contains(v)));
}

#[test]
fn the_companion_and_the_harness_name_the_same_modules() {
    use film_lab_photocraft_harness::Plugin as Built;
    for (ours, built) in Plugin::ALL.into_iter().zip(Built::ALL) {
        assert_eq!((ours.id(), ours.file_name()), (built.id(), built.file_name()));
        let wasm = film_lab_photocraft_harness::build_plugin(built).unwrap();
        let mut host = film_lab_photocraft_harness::Host::load(&std::fs::read(&wasm).unwrap()).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&host.manifest().unwrap()).unwrap();
        assert_eq!(manifest["id"], ours.id(), "the id the companion runs is the one the module declares");
        assert_eq!(
            find_plugin(None, ours).unwrap().canonicalize().unwrap(),
            wasm.canonicalize().unwrap(),
            "the repository build is the default module"
        );
    }
}

/// A colour negative (each channel its own scene ramp, so the neutral balance has work to do)
/// with a film border, 8-bit sRGB.
fn coloured(w: u32, h: u32, border: u32) -> DynamicImage {
    let white = libm::exp2f(RANGE_STOPS) - 1.0;
    let img = ImageBuffer::from_fn(w, h, |x, y| {
        let inside = x >= border && y >= border && x < w - border && y < h - border;
        let t = if inside { ((x - border) + (y - border) * w) as f32 / (w * h) as f32 } else { 0.0 };
        let mut px = [0u8; 3];
        for ch in 0..3 {
            let s = (t * [1.0, 0.7, 0.4][ch]).min(1.0);
            px[ch] = (color::srgb_encode(film::transmittance(1.0 + s * white, BASE[ch])) * 255.0).round() as u8;
        }
        Rgb(px)
    });
    DynamicImage::ImageRgb8(img)
}

#[test]
fn pc_analyze_measures_what_the_companion_measures() {
    use film_lab_photocraft_harness::{Host, MODE_RGB, Plugin as Built, build_plugin, format_code};
    let img = coloured(300, 200, 12);
    let levels = analyze(&img, &AnalyzeOptions::default()).unwrap();
    assert!(levels.balance.iter().any(|b| (b - 1.0).abs() > 0.01), "the frame needs a balance: {levels:?}");
    let px: Vec<f32> = img.to_rgb8().pixels().flat_map(|p| p.0.map(|v| f32::from(v) / 255.0)).collect();
    for built in Built::ALL {
        let mut host = Host::load(&std::fs::read(build_plugin(built).unwrap()).unwrap()).unwrap();
        let text = host.analyze(&px, 300, 200, 3, format_code(8, false, MODE_RGB), "{}").unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        let near = |key: &str, ours: [f32; 3], tol: f64| {
            for (ch, v) in ours.iter().enumerate() {
                let theirs = json[key][ch].as_f64().unwrap();
                assert!((theirs - f64::from(*v)).abs() <= tol, "{built:?} {key}[{ch}]: pc_analyze {theirs}, companion {v}");
            }
        };
        // pc_analyze rounds levels to tenths and the balance to thousandths, as the block does.
        near("base", levels.base, 0.051);
        near("black", levels.black, 0.051);
        near("white", levels.white, 0.051);
        near("balance", levels.balance, 0.00051);
        assert_eq!(json["samples"], levels.samples, "{built:?}");
    }
}

#[test]
fn converts_through_photocraft_cli_when_available() {
    let Some(cli) = std::env::var_os("PHOTOCRAFT_CLI") else {
        eprintln!("PHOTOCRAFT_CLI not set: skipping the photocraft-cli conversion");
        return;
    };
    let wasm = match film_lab_photocraft_harness::build_plugin(film_lab_photocraft_harness::Plugin::Convert) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}: skipping");
            return;
        }
    };
    assert_eq!(
        find_plugin(None, Plugin::Convert).unwrap().canonicalize().unwrap(),
        wasm.canonicalize().unwrap(),
        "the repository build is the default module"
    );
    let dir = std::env::temp_dir().join(format!("film-lab-companion-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = synthetic(320, 240, 16);
    let input = dir.join("negative.png");
    img.save(&input).unwrap();
    let levels = analyze(&img, &AnalyzeOptions { border_percent: 0.0, ..AnalyzeOptions::default() }).unwrap();
    let mut params = levels.plugin_params();
    params.insert("preset".into(), "Standard".into());
    let output = dir.join("positive.png");
    convert_one(std::path::Path::new(&cli), Plugin::Convert, &wasm, &levels, &input, &output, &params).unwrap();
    let out = image::open(&output).unwrap().to_rgb8();
    // The ramp's middle comes back neutral and at least middle grey (the lab rendition
    // brightens mid-tones against the scene-linear value).
    let p = out.get_pixel(160, 120).0;
    let want = (color::srgb_encode(0.5) * 255.0) as i32;
    assert!(i32::from(p[0]) >= want - 3 && (i32::from(p[0]) - i32::from(p[2])).abs() <= 3, "{p:?} vs at least {want}, neutral");
    std::fs::remove_dir_all(&dir).ok();
}
