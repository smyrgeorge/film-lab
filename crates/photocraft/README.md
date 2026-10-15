# Film Lab for PhotoCraft

WebAssembly filter plug-ins for PhotoCraft's plug-in ABI v1 (`photocraft/docs/plugins.md`),
built like the upstream example `examples/plugins/invert-rs`: `no_std`, no allocator, no imports.
Each converts a scanned film negative on the active layer into a positive.

An ABI v1 module is one manifest and one menu entry, so each plug-in is its own module:

| Module | Menu entry (Filter › Plug-ins) | Id | Dialog |
|---|---|---|---|
| `film_lab_convert.wasm` | Film Lab: Convert Negative… | `io.github.smyrgeorge.film-lab.convert` | a film preset and six sliders |
| `film_lab_convert_advanced.wasm` | Film Lab: Convert Negative (Advanced)… | `io.github.smyrgeorge.film-lab.convert-advanced` | the film base picked from the border, tone, white balance, a neutral sample, colour and sharpening |

## Layout

This directory is **its own cargo workspace** (not a member of the repository workspace): it
only builds for `wasm32-unknown-unknown`, and `.cargo/config.toml` makes that the default target
here. The modules share one version (`[workspace.package]`), which every manifest reports.

```text
abi/        film-lab-photocraft: the four exports (`plugin!`), the band, the measurement block,
            the conversion loop and sharpening, shared by every plug-in
convert/    film-lab-convert → film_lab_convert.wasm: its manifest and its parameters
advanced/   film-lab-convert-advanced → film_lab_convert_advanced.wasm: the same, with every control
```

A new plug-in is a crate next to `convert/`: a manifest, and a function from its parameters to
`film_lab_core` settings, handed to `film_lab_photocraft::plugin!`.

## Build

```sh
rustup target add wasm32-unknown-unknown
./build.sh        # cargo build --release --target wasm32-unknown-unknown
```

Output: `target/wasm32-unknown-unknown/release/film_lab_convert.wasm` and
`film_lab_convert_advanced.wasm`. A release (`.github/workflows/release.yml`) attaches both.

## Install

```text
plugin.install {"path": "/abs/path/film_lab_convert.wasm"}
plugin.install {"path": "/abs/path/film_lab_convert_advanced.wasm"}
```

Or copy the `.wasm` files into the folder set in Edit › Preferences › Plug-ins (with *Use
Additional Plug-ins Folder* on). Each appears under **Filter › Plug-ins** with a generated
dialog and live preview. Install either or both; the ids differ, so neither replaces the other.

## Workflow

Screenshots of these steps in the app are in the [repository README](../../README.md#use-in-photocraft).

**Automatic** (the companion, `crates/companion`): with PhotoCraft started on its control channel
and the scan open, `film-lab app` measures the whole document, writes the measurement into the
module it installs, and runs the plug-in; with `--dialog` it opens the dialog instead, for
choosing the preset and adjusting the sliders with the live preview before OK. `--advanced` does
the same with the advanced plug-in.

**In the dialog alone**: Filter › Plug-ins › Film Lab: Convert Negative…, pick a preset, adjust.
The dialog carries whatever measurement the module holds (the companion's last one); with none,
the preset's typical mask colour stands in for the film base. In the advanced dialog, type the
border's colour into Film Base Red / Green / Blue instead.

A conversion is one undo step; on a smart object it is recorded as a smart filter and re-run on
re-render.

## Parameters

### Convert Negative

| Parameter | Range | What |
|---|---|---|
| `preset` | Standard (default), Portra 160, Portra 400, Ektar 100, Gold 200, ColorPlus 200, UltraMax 400, Superia 400, C200, Pro 400H, CineStill 800, Monochrome | The film's character: layer contrast, mask colour, tone profile, colour model, saturation, warmth, toning (`crates/core/src/preset.rs`) |
| `exposure` | −5..5 stops | Overall brightness |
| `brightness` | −50..50 | Mid-tone gamma; black and white stay put |
| `contrast` | −50..50 | Added to the preset's contrast |
| `temp`, `tint` | −50..50 | Warmer / cooler, magenta / green, on top of the preset's |
| `saturation` | −50..50 | On top of the preset's |

Not in the dialog, but read from the JSON when a host passes them: `base_r/g/b` (0..255, the Info
panel's values on the unexposed border, replacing the preset's base when the module holds no
measurement), `neutral_r/g/b` (a scan colour that should come out grey), `range` (stops rendered
between black and white; 3 by default, 6 is scene-linear), and `encoding` (`srgb` or `linear`;
otherwise 8- and 16-bit documents are sRGB, 32-bit linear).

### Convert Negative (Advanced)

The same preset and sliders, and in dialog order (the dialog's label, then the key):

| Control | Key | Range | What |
|---|---|---|---|
| Film Base Red / Green / Blue | `filmBaseRed`, `filmBaseGreen`, `filmBaseBlue` | whole numbers 0..255, 0 = unset | The film base: the unexposed border as the Info panel shows it (sample it with the Eyedropper, or `film-lab sample`). Set, it wins over the measured base and the preset's; a measurement's white point and balance still apply. A grey document reads Film Base Red |
| Tone Curve | `toneCurve` | From Preset (default), Lab Standard, Lab Soft, Lab Hard, Lab Highlight Soft, Lab Highlight Hard, Lab Shadow Soft, Lab Shadow Hard, Linear, Linear Deep, Linear Flat, Cinematic, Cinematic Flat, Cinematic Rich | The tone curve, instead of the preset's |
| Highlights, Shadows | `highlights`, `shadows` | −50..50 | Brighten the lighter / darker tones |
| Whites, Blacks | `whites`, `blacks` | −50..50 | Move the white / black point |
| Fade, Glow | `fade`, `glow` | −100..100 | Lift black for smoother shadows / compress the highlights into a shoulder |
| Dynamic Range | `dynamicRange` | 1..10 stops, default 3 | Scene stops between black and white; fewer renders mid-tones brighter, 6 is scene-linear |
| White Balance | `whiteBalance` | As Shot, Warm, Cool, Tungsten Film | A white-balance preset, under Temp and Tint |
| Gray Point Red / Green / Blue | `grayPointRed`, `grayPointGreen`, `grayPointBlue` | whole numbers 0..255, 0 = unset | A scan colour that should come out grey |
| Color Look | `colorLook` | From Preset (default), Neutral, Lab Warm, Lab Cool, Print, Clean | The colour model, instead of the preset's |
| Sharpening, Amount | `sharpening`, `amount` | Off (default), Lab, Scanner; 0..100 % | Output sharpening: Lab leaves grain alone, Scanner sharpens everything. The manifest asks for a pixel of overlap so bands join seamlessly |

Left at its defaults, the advanced plug-in converts exactly as Convert Negative does (the harness
checks it on every preset, with and without a measurement).

PhotoCraft writes a control's label from its key, capitalising it and splitting camelCase into
words, and shows a menu option with a space exactly as written. So the keys are camelCase and
the options are the menu's text, which `advanced/src/lib.rs` maps to the core's values; a key
named `amount` gets a % unit. Its plug-in dialogs have numbers, checkboxes and lists but no
colour picker, so a colour is three whole numbers on the Info panel's scale. The harness checks
every label and option against PhotoCraft's rule.

### The measurement, or the preset's base

ABI v1 runs every band of the layer (a strip 256 rows tall, up to 1280 px wide for RGB) in a
fresh instance with no whole-image pass, the dialog's preview included, so anything measured
from the pixels a band holds differs from band to band and shows as blocks. The plug-ins
therefore measure nothing themselves.

- **With a measurement** in the module (the companion's: film base, black and white points,
  balance, all measured on the whole scan) nothing depends on the band, so large layers
  convert without seams.
- **Without one**, the preset's typical mask colour (or the base picked in the advanced dialog)
  is the base and a fixed range of stops leads to white: identical on every band, seam-free, right when the scanning setup matches.

### The measurement block

The host drops parameters a manifest doesn't declare, so the measurement can't be a hidden
parameter. Every module has a 512-byte block starting with `FILMLAB-MEASURE-1:`; the companion
writes the measurement there as JSON (`base_*`, `black_*`, `white_*` as scan values 0..255,
`balance_*`) before installing the module, and the plug-in reads it (`abi/`). The harness tests patch the
block the same way.

## The conversion

Clean-room, from the physics of negatives (`crates/core`): density grows by the layer's gamma
(about 0.6 for colour negative film, per layer in the presets) per stop of scene exposure, so
relative exposure is `(base / scan)^(1/γ)` per channel, with the border as base. Dividing by the base per channel also neutralises the orange mask. Levels work in
density space: each channel's range between its black and white points is mapped onto the same
`range` of stops, so a scan whose blue runs far denser than its red (every 8-bit scan of an
orange-masked negative) still comes out balanced, with black and white at 0 and 1. White
balance, the colour model, pre-saturation, mid-tone balance, exposure and the white / black
points act in linear light; brightness and the per-channel balance are gammas; the tone
profile, contrast, lights / darks, fade / glow and the soft ends act in sRGB-encoded space, then
the toning and saturation, then sharpening.

Measured with `film-lab compare`: on a lab-scanned Portra 400 frame with its lab positive,
the companion's analysis with a border buffer of 8 % and the defaults lands every channel's
mean within about 1 %; on an openly licensed Ektar 100 frame photographed against daylight (the
README's example), `--wb neutral` or a neutral sample on a cloud lands within about 4 % of the
author's own positive.

The host meters about 4,000 instructions per sample and runs the module in an interpreter, so
the conversion samples each channel's transfer functions into lookup tables once per band
(`film_lab_core::Tables`, 48 KiB in static memory) and interpolates per sample: no `powf` per
pixel, and a positive within a thousandth of the exact one. The harness tests check the budget
and the core's tests the accuracy.

## Return codes

| Code | Meaning |
|---|---|
| 0 | converted |
| 1 | no colour channel (alpha only) |
| 2 | unsupported colour mode (CMYK / multichannel) |

## Test

`cargo test -p film-lab-photocraft-harness` (from the repository root) builds these modules and
drives their exports with wasmi, the interpreter PhotoCraft embeds. `tests/abi.rs` checks every
module: ABI version, a manifest the host accepts (distinct ids, the workspace's version, legal
parameter names), one measurement block, hostile input and the instruction budget.
`tests/convert.rs` round-trips negatives synthesised from the core's own film model through
Convert Negative (a typed base, the embedded measurement and the fallback without one, greyscale,
linear documents, every preset and slider, the neutral sample). `tests/advanced.rs` checks that
the advanced plug-in at its defaults matches Convert Negative, that a picked base neutralises a
wrong mask and wins over the measured one, and its tone, colour and sharpening controls. The
`synthetic_negative` example writes a negative to run through `photocraft-cli` by hand.

## `pc_analyze` (ABI v1.1 proposal)

Both modules also export `pc_analyze`, which runs the companion's measurement on a whole frame a
host hands over and returns it as JSON; a host passes it back to `pc_filter` as `_analysis`,
which wins over the embedded measurement. PhotoCraft doesn't call it, so nothing changes there.
See `PROPOSAL-pc-analyze.md`.
