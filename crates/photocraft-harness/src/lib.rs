//! Host-side test harness for the PhotoCraft plug-ins (`crates/photocraft`).
//!
//! Builds the plug-ins for `wasm32-unknown-unknown` and drives their ABI v1 exports through
//! [`wasmi`], the interpreter PhotoCraft itself embeds, so the contract (no imports, the four
//! exports, the manifest, `pc_alloc`, `pc_filter`) is checked without a PhotoCraft checkout. For
//! the host's full behaviour (limits, band splitting, selection blend) run a module through
//! PhotoCraft's `plugin.install` / `plugin.run` or its `photocraft-plugins` crate.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use wasmi::{Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// The plug-ins' workspace (`crates/photocraft`).
pub fn plugin_crate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../photocraft")
}

/// One plug-in module of `crates/photocraft`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plugin {
    /// Film Lab: Convert Negative (`crates/photocraft/convert`).
    Convert,
    /// Film Lab: Convert Negative (Advanced) (`crates/photocraft/advanced`).
    Advanced,
}

impl Plugin {
    pub const ALL: [Plugin; 2] = [Plugin::Convert, Plugin::Advanced];

    /// The module's file name.
    pub fn file_name(self) -> &'static str {
        match self {
            Plugin::Convert => "film_lab_convert.wasm",
            Plugin::Advanced => "film_lab_convert_advanced.wasm",
        }
    }

    /// The id its manifest declares.
    pub fn id(self) -> &'static str {
        match self {
            Plugin::Convert => "io.github.smyrgeorge.film-lab.convert",
            Plugin::Advanced => "io.github.smyrgeorge.film-lab.convert-advanced",
        }
    }
}

/// Builds the plug-ins in release mode and returns the path of `plugin`'s `.wasm`. Built once
/// per process: tests run in parallel and must not race each other's `cargo build`.
///
/// Runs the plug-ins' own cargo workspace with its own target directory, so an outer
/// `cargo test` (which holds the lock on the repository's target directory) can't deadlock it.
pub fn build_plugin(plugin: Plugin) -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let dir = BUILT.get_or_init(build_plugins).clone()?;
    let wasm = dir.join(plugin.file_name());
    if !wasm.is_file() {
        return Err(format!("{} was not produced", wasm.display()));
    }
    Ok(wasm)
}

/// Builds every module; the directory they land in.
fn build_plugins() -> Result<PathBuf, String> {
    let dir = plugin_crate_dir();
    let target_dir = dir.join("target");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET")
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err(format!("building the plug-ins failed ({status}); is the wasm32-unknown-unknown target installed?"));
    }
    Ok(target_dir.join("wasm32-unknown-unknown/release"))
}

/// The marker that starts a plug-in's measurement block, and the block's size.
pub const MEASUREMENT_MARKER: &[u8] = b"FILMLAB-MEASURE-1:";
pub const MEASUREMENT_LEN: usize = 512;

/// `wasm` with `json` written into its measurement block (padded with spaces), as the companion
/// does before installing the plug-in.
pub fn embed_measurement(wasm: &[u8], json: &str) -> Result<Vec<u8>, String> {
    let at = wasm.windows(MEASUREMENT_MARKER.len()).position(|w| w == MEASUREMENT_MARKER).ok_or("the module has no measurement block")?;
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

/// Pixel `format` code as the host builds it: `depth | alpha << 8 | mode << 16`.
pub fn format_code(depth: u32, alpha: bool, mode: u32) -> u32 {
    depth | (u32::from(alpha) << 8) | (mode << 16)
}

/// The RGB colour mode number (PSD numbering, as the ABI uses).
pub const MODE_RGB: u32 = 3;
/// The Grayscale colour mode number.
pub const MODE_GRAY: u32 = 1;
/// The CMYK colour mode number.
pub const MODE_CMYK: u32 = 4;

/// `pc_filter(buf, buf_len, width, height, channels, format, params, params_len) -> i32`.
type FilterFn = TypedFunc<(i32, i32, i32, i32, i32, i32, i32, i32), i32>;
/// `pc_analyze(…same arguments…) -> i64` (ABI v1.1 proposal).
type AnalyzeFn = TypedFunc<(i32, i32, i32, i32, i32, i32, i32, i32), i64>;

/// PhotoCraft's instruction budget: this much per call …
pub const FUEL_BASE: u64 = 50_000_000;
/// … plus this much per sample of `pc_filter` (`photocraft_plugins::Limits`).
pub const FUEL_PER_SAMPLE: u64 = 4_000;

/// One instantiated plug-in module and its ABI v1 exports.
pub struct Host {
    store: Store<()>,
    memory: Memory,
    abi_version: TypedFunc<(), i32>,
    manifest: TypedFunc<(), i64>,
    alloc: TypedFunc<i32, i32>,
    filter: FilterFn,
    instance: Instance,
    fuel_used: u64,
}

impl Host {
    /// Validates and instantiates `wasm`, checking the ABI's shape: no imports, a `memory`
    /// export and the four functions with their exact signatures.
    pub fn load(wasm: &[u8]) -> Result<Self, String> {
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, wasm).map_err(|e| format!("invalid module: {e}"))?;
        let imports: Vec<String> = module.imports().map(|i| format!("{}.{}", i.module(), i.name())).collect();
        if !imports.is_empty() {
            return Err(format!("the module imports {imports:?}; ABI v1 modules must import nothing"));
        }
        let mut store = Store::new(&engine, ());
        let linker = <Linker<()>>::new(&engine);
        let instance: Instance = linker.instantiate_and_start(&mut store, &module).map_err(|e| format!("instantiate: {e}"))?;
        let memory = instance.get_memory(&store, "memory").ok_or("no `memory` export")?;
        let abi_version = instance.get_typed_func(&store, "pc_abi_version").map_err(|e| format!("pc_abi_version: {e}"))?;
        let manifest = instance.get_typed_func(&store, "pc_manifest").map_err(|e| format!("pc_manifest: {e}"))?;
        let alloc = instance.get_typed_func(&store, "pc_alloc").map_err(|e| format!("pc_alloc: {e}"))?;
        let filter = instance.get_typed_func(&store, "pc_filter").map_err(|e| format!("pc_filter: {e}"))?;
        let mut host = Self { store, memory, abi_version, manifest, alloc, filter, instance, fuel_used: 0 };
        host.refuel(FUEL_BASE)?;
        Ok(host)
    }

    fn refuel(&mut self, fuel: u64) -> Result<(), String> {
        self.store.set_fuel(fuel).map_err(|e| format!("fuel: {e}"))
    }

    fn fuel_left(&self) -> Result<u64, String> {
        self.store.get_fuel().map_err(|e| format!("fuel: {e}"))
    }

    /// Instructions the last `filter` call used.
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
    }

    pub fn abi_version(&mut self) -> Result<i32, String> {
        self.abi_version.call(&mut self.store, ()).map_err(|e| e.to_string())
    }

    /// The manifest JSON text.
    pub fn manifest(&mut self) -> Result<String, String> {
        let packed = self.manifest.call(&mut self.store, ()).map_err(|e| e.to_string())? as u64;
        let (len, ptr) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        let mut bytes = vec![0u8; len];
        self.memory.read(&self.store, ptr, &mut bytes).map_err(|e| format!("manifest out of range: {e}"))?;
        String::from_utf8(bytes).map_err(|e| format!("manifest is not UTF-8: {e}"))
    }

    /// `pc_alloc(size)`: the offset of a writable block, or an error when the module returned 0.
    pub fn alloc(&mut self, size: usize) -> Result<usize, String> {
        let size = i32::try_from(size).map_err(|_| "size too large".to_string())?;
        let ptr = self.alloc.call(&mut self.store, size).map_err(|e| e.to_string())?;
        usize::try_from(ptr).ok().filter(|p| *p != 0).ok_or_else(|| "pc_alloc returned 0".to_string())
    }

    /// Runs `pc_filter` on `pixels` (interleaved `f32`, `width × height × channels`) in place,
    /// with `params` as the parameter JSON. Returns the plug-in's return code.
    pub fn filter(&mut self, pixels: &mut [f32], width: u32, height: u32, channels: u32, format: u32, params: &str) -> Result<i32, String> {
        let budget = FUEL_BASE + FUEL_PER_SAMPLE * pixels.len() as u64;
        self.refuel(budget)?;
        let bytes: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
        let buf = self.alloc(bytes.len().max(1))?;
        self.memory.write(&mut self.store, buf, &bytes).map_err(|e| format!("write pixels: {e}"))?;
        let pbuf = self.alloc(params.len().max(1))?;
        self.memory.write(&mut self.store, pbuf, params.as_bytes()).map_err(|e| format!("write params: {e}"))?;
        let arg = |v: usize| i32::try_from(v).map_err(|_| "argument too large".to_string());
        let code = self
            .filter
            .call(
                &mut self.store,
                (arg(buf)?, arg(bytes.len())?, width as i32, height as i32, channels as i32, format as i32, arg(pbuf)?, arg(params.len())?),
            )
            .map_err(|e| format!("pc_filter trapped: {e}"))?;
        self.fuel_used = budget.saturating_sub(self.fuel_left()?);
        let mut out = vec![0u8; bytes.len()];
        self.memory.read(&self.store, buf, &mut out).map_err(|e| format!("read pixels: {e}"))?;
        for (v, b) in pixels.iter_mut().zip(out.as_chunks::<4>().0) {
            *v = f32::from_le_bytes(*b);
        }
        Ok(code)
    }

    /// Runs the optional `pc_analyze` export (ABI v1.1 proposal) on `pixels`: the JSON text it
    /// returned, `None` for 0 (nothing measured), or an error when the module lacks the export.
    pub fn analyze(&mut self, pixels: &[f32], width: u32, height: u32, channels: u32, format: u32, params: &str) -> Result<Option<String>, String> {
        let f: AnalyzeFn = self.instance.get_typed_func(&self.store, "pc_analyze").map_err(|e| format!("pc_analyze: {e}"))?;
        self.refuel(FUEL_BASE + FUEL_PER_SAMPLE * pixels.len() as u64)?;
        let bytes: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
        let buf = self.alloc(bytes.len().max(1))?;
        self.memory.write(&mut self.store, buf, &bytes).map_err(|e| format!("write pixels: {e}"))?;
        let pbuf = self.alloc(params.len().max(1))?;
        self.memory.write(&mut self.store, pbuf, params.as_bytes()).map_err(|e| format!("write params: {e}"))?;
        let arg = |v: usize| i32::try_from(v).map_err(|_| "argument too large".to_string());
        let packed = f
            .call(
                &mut self.store,
                (arg(buf)?, arg(bytes.len())?, width as i32, height as i32, channels as i32, format as i32, arg(pbuf)?, arg(params.len())?),
            )
            .map_err(|e| format!("pc_analyze trapped: {e}"))? as u64;
        if packed == 0 {
            return Ok(None);
        }
        let (len, ptr) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        let mut text = vec![0u8; len];
        self.memory.read(&self.store, ptr, &mut text).map_err(|e| format!("analysis out of range: {e}"))?;
        String::from_utf8(text).map(Some).map_err(|e| format!("analysis is not UTF-8: {e}"))
    }
}
