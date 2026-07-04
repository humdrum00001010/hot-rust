//! M5: compose the M3/M4/M2 pieces into an end-to-end native hot-reload slice.
//!
//! Build so the target app's registered functions start with patch padding, and
//! so the rust-analyzer crates can compile on this toolchain:
//!   RUSTC_BOOTSTRAP=1 \
//!   RUSTFLAGS="-Zpatchable-function-entry=16 -Zcrate-attr=feature(if_let_guard)" \
//!   cargo run --features m3-oracle --bin m5
//!
//! This is a small native render/layout target rather than a synthetic `target()`.
//! The driver classifies a body-only source edit, resolves the edited source item
//! to the live function entry, builds/loads a patch dylib, writes the prologue
//! jump, and proves the next direct render call uses the new body without restart.

#[path = "m3.rs"]
#[allow(dead_code)]
mod m3_oracle;
#[path = "m4.rs"]
#[allow(dead_code)]
mod m4_runtime;

use m3_oracle::{PatchTarget, Route};
use std::error::Error;
use std::fmt;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const TARGET_PATH: &str = "layout::render_page_svg_native";
const TARGET_SIGNATURE: &str = "extern \"C\" fn(PageInput) -> LayoutMetrics";

const OLD_TARGET_SOURCE: &str = r#"
#[repr(C)]
pub struct PageInput {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub margin: u32,
}

#[repr(C)]
pub struct LayoutMetrics {
    pub content_width: u32,
    pub content_height: u32,
    pub band_count: u32,
}

pub mod layout {
    use super::{LayoutMetrics, PageInput};

    pub extern "C" fn render_page_svg_native(input: PageInput) -> LayoutMetrics {
        let content_width = input.width - input.margin * 2;
        let content_height = input.height - input.margin * 2;
        let band_count = content_height / 128 + 1;
        LayoutMetrics { content_width, content_height, band_count }
    }

    pub extern "C" fn stable_checksum(input: PageInput) -> u32 {
        input.page + input.width + input.height + input.margin
    }
}
"#;

const NEW_TARGET_SOURCE: &str = r#"
#[repr(C)]
pub struct PageInput {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub margin: u32,
}

#[repr(C)]
pub struct LayoutMetrics {
    pub content_width: u32,
    pub content_height: u32,
    pub band_count: u32,
}

pub mod layout {
    use super::{LayoutMetrics, PageInput};

    pub extern "C" fn render_page_svg_native(input: PageInput) -> LayoutMetrics {
        let content_width = input.width - input.margin * 4;
        let content_height = input.height - input.margin * 3;
        let band_count = content_height / 64 + 1;
        LayoutMetrics { content_width, content_height, band_count }
    }

    pub extern "C" fn stable_checksum(input: PageInput) -> u32 {
        input.page + input.width + input.height + input.margin
    }
}
"#;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageInput {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub margin: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutMetrics {
    pub content_width: u32,
    pub content_height: u32,
    pub band_count: u32,
}

type PatchFn = unsafe extern "C" fn(PageInput) -> LayoutMetrics;

pub mod layout {
    use super::{LayoutMetrics, PageInput};
    use std::hint::black_box;

    #[inline(never)]
    pub extern "C" fn render_page_svg_native(input: PageInput) -> LayoutMetrics {
        let input = black_box(input);
        let content_width = input.width - input.margin * 2;
        let content_height = input.height - input.margin * 2;
        let band_count = content_height / 128 + 1;
        black_box(LayoutMetrics {
            content_width,
            content_height,
            band_count,
        })
    }

    #[inline(never)]
    pub extern "C" fn stable_checksum(input: PageInput) -> u32 {
        let input = black_box(input);
        black_box(input.page + input.width + input.height + input.margin)
    }
}

#[derive(Debug, Clone)]
struct PatchIntent {
    source_path: String,
    patch_export: String,
    signature_key: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct LiveSymbol {
    source_path: &'static str,
    patch_export: &'static str,
    signature_key: &'static str,
    old_addr: usize,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedSymbol {
    source_path: &'static str,
    patch_export: &'static str,
    signature_key: &'static str,
    old_addr: usize,
}

#[derive(Debug)]
enum PipelineError {
    NoPatchTargets,
    MultiplePatchTargets(Vec<PatchTarget>),
    UnexpectedRoute(Route),
    MissingPath {
        source_path: String,
    },
    ExportMismatch {
        source_path: String,
        expected: String,
        actual: &'static str,
    },
    SignatureMismatch {
        source_path: String,
        expected: &'static str,
        actual: &'static str,
    },
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPatchTargets => write!(f, "oracle returned BodyOnly with no patch targets"),
            Self::MultiplePatchTargets(targets) => {
                write!(f, "M5 expects one edited function, got {targets:?}")
            }
            Self::UnexpectedRoute(route) => {
                write!(f, "expected body-only patch route, got {route}")
            }
            Self::MissingPath { source_path } => {
                write!(f, "no live symbol registered for source path {source_path}")
            }
            Self::ExportMismatch {
                source_path,
                expected,
                actual,
            } => write!(
                f,
                "source path {source_path} resolved to patch export {actual}, expected {expected}"
            ),
            Self::SignatureMismatch {
                source_path,
                expected,
                actual,
            } => write!(
                f,
                "source path {source_path} resolved to signature {actual}, expected {expected}"
            ),
        }
    }
}

impl Error for PipelineError {}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let input = PageInput {
        page: 3,
        width: 800,
        height: 1100,
        margin: 20,
    };

    println!("target frame before edit: {}", render_frame(input));
    println!(
        "stable checksum before edit: {}",
        black_box(layout::stable_checksum(black_box(input)))
    );

    let route = m3_oracle::classify_edit(OLD_TARGET_SOURCE, NEW_TARGET_SOURCE);
    println!("M3 route: {route}");
    let patch_target = expect_single_patch_target(route)?;
    if patch_target.path != TARGET_PATH {
        return Err(format!(
            "oracle selected {}, expected {TARGET_PATH}",
            patch_target.path
        )
        .into());
    }

    let intent = PatchIntent {
        source_path: patch_target.path,
        patch_export: patch_target.export_symbol,
        signature_key: TARGET_SIGNATURE,
    };
    println!(
        "driver intent: source_path={} patch_export={} signature={}",
        intent.source_path, intent.patch_export, intent.signature_key
    );

    let registry = live_registry();
    let resolved = resolve_live_symbol(&registry, &intent)?;
    println!(
        "M4 resolved: {} -> old_addr={:#x}, export={}, sig={}",
        resolved.source_path, resolved.old_addr, resolved.patch_export, resolved.signature_key
    );

    unsafe {
        let entry = core::slice::from_raw_parts(resolved.old_addr as *const u8, 16);
        println!("resolved entry bytes (expect NOP padding): {entry:02x?}");
    }

    let patch = build_patch_dylib(resolved.patch_export)?;
    println!("patch dylib: {}", patch.dylib.display());

    let library = unsafe { m4_runtime::dylib::Library::open(&patch.dylib)? };
    let new_addr = unsafe { library.symbol(resolved.patch_export)? as usize };
    let replacement: PatchFn = unsafe { std::mem::transmute(new_addr) };
    println!(
        "patch export {} = {new_addr:#x}, direct dylib render = {}",
        resolved.patch_export,
        frame_from_metrics(input, unsafe { replacement(input) })
    );

    unsafe {
        m4_runtime::patch_to_external(resolved.old_addr, new_addr)?;
    }

    let after_frame = render_frame(input);
    let stable_after = black_box(layout::stable_checksum(black_box(input)));
    println!("target frame after patch: {after_frame}");
    println!("stable checksum after patch: {stable_after}");

    assert_eq!(
        after_frame, "page=3 content=720x1040 bands=17",
        "M5 failed: next render did not use the patched layout body"
    );
    assert_eq!(
        stable_after, 1923,
        "M5 patched or corrupted an unrelated target-app function"
    );
    println!("OK: M5 applied a body-only layout edit to the running native render target without restart.");

    drop(library);
    drop(patch);
    Ok(())
}

fn expect_single_patch_target(route: Route) -> Result<PatchTarget, PipelineError> {
    match route {
        Route::BodyOnly { targets } => {
            let [target] = targets.as_slice() else {
                return if targets.is_empty() {
                    Err(PipelineError::NoPatchTargets)
                } else {
                    Err(PipelineError::MultiplePatchTargets(targets))
                };
            };
            Ok(target.clone())
        }
        other => Err(PipelineError::UnexpectedRoute(other)),
    }
}

fn live_registry() -> Vec<LiveSymbol> {
    vec![
        LiveSymbol {
            source_path: TARGET_PATH,
            patch_export: "hot_rust_patch_layout_render_page_svg_native",
            signature_key: TARGET_SIGNATURE,
            old_addr: layout::render_page_svg_native as *const () as usize,
        },
        LiveSymbol {
            source_path: "layout::stable_checksum",
            patch_export: "hot_rust_patch_layout_stable_checksum",
            signature_key: "extern \"C\" fn(PageInput) -> u32",
            old_addr: layout::stable_checksum as *const () as usize,
        },
    ]
}

fn resolve_live_symbol(
    registry: &[LiveSymbol],
    intent: &PatchIntent,
) -> Result<ResolvedSymbol, PipelineError> {
    let Some(symbol) = registry
        .iter()
        .copied()
        .find(|symbol| symbol.source_path == intent.source_path)
    else {
        return Err(PipelineError::MissingPath {
            source_path: intent.source_path.clone(),
        });
    };

    if symbol.patch_export != intent.patch_export {
        return Err(PipelineError::ExportMismatch {
            source_path: intent.source_path.clone(),
            expected: intent.patch_export.clone(),
            actual: symbol.patch_export,
        });
    }

    if symbol.signature_key != intent.signature_key {
        return Err(PipelineError::SignatureMismatch {
            source_path: intent.source_path.clone(),
            expected: intent.signature_key,
            actual: symbol.signature_key,
        });
    }

    Ok(ResolvedSymbol {
        source_path: symbol.source_path,
        patch_export: symbol.patch_export,
        signature_key: symbol.signature_key,
        old_addr: symbol.old_addr,
    })
}

fn render_frame(input: PageInput) -> String {
    let metrics = black_box(layout::render_page_svg_native(black_box(input)));
    frame_from_metrics(input, metrics)
}

fn frame_from_metrics(input: PageInput, metrics: LayoutMetrics) -> String {
    format!(
        "page={} content={}x{} bands={}",
        input.page, metrics.content_width, metrics.content_height, metrics.band_count
    )
}

struct BuiltPatch {
    root: PathBuf,
    dylib: PathBuf,
}

impl Drop for BuiltPatch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn build_patch_dylib(export_symbol: &str) -> Result<BuiltPatch, Box<dyn Error>> {
    validate_rust_export_identifier(export_symbol)?;

    let root = std::env::temp_dir().join(format!(
        "hot-rust-m5-patch-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir)?;

    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "hot-rust-m5-patch"
version = "0.1.0"
edition = "2021"

[lib]
name = "hot_rust_m5_patch"
crate-type = ["cdylib"]

[profile.dev]
opt-level = 0
"#,
    )?;
    fs::write(src_dir.join("lib.rs"), patch_library_source(export_symbol))?;

    let mut command = Command::new(cargo_command());
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", "-Zpatchable-function-entry=16");

    println!("building patch crate with cargo...");
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "patch cargo build failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let dylib = root
        .join("target")
        .join("debug")
        .join(dylib_filename("hot_rust_m5_patch"));
    if !dylib.exists() {
        return Err(format!("patch dylib was not produced at {}", dylib.display()).into());
    }

    Ok(BuiltPatch { root, dylib })
}

fn patch_library_source(export_symbol: &str) -> String {
    format!(
        r#"#[repr(C)]
#[derive(Clone, Copy)]
pub struct PageInput {{
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub margin: u32,
}}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LayoutMetrics {{
    pub content_width: u32,
    pub content_height: u32,
    pub band_count: u32,
}}

#[no_mangle]
pub extern "C" fn {export_symbol}(input: PageInput) -> LayoutMetrics {{
    let input = std::hint::black_box(input);
    let content_width = input.width - input.margin * 4;
    let content_height = input.height - input.margin * 3;
    let band_count = content_height / 64 + 1;
    std::hint::black_box(LayoutMetrics {{
        content_width,
        content_height,
        band_count,
    }})
}}
"#
    )
}

fn validate_rust_export_identifier(symbol: &str) -> Result<(), Box<dyn Error>> {
    let mut chars = symbol.chars();
    let Some(first) = chars.next() else {
        return Err("empty patch export symbol".into());
    };

    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(format!("patch export is not a Rust identifier: {symbol}").into());
    }

    if chars.any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric())) {
        return Err(format!("patch export is not a Rust identifier: {symbol}").into());
    }

    Ok(())
}

fn cargo_command() -> std::ffi::OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn dylib_filename(name: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        format!("lib{name}.dylib")
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        format!("lib{name}.so")
    }

    #[cfg(windows)]
    {
        format!("{name}.dll")
    }
}
