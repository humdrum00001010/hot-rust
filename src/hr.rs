//! M6: `hr cargo ...` supervisor slice.
//!
//! `hr` starts the hot session before invoking Cargo. For this slice it:
//!
//! - resolves the Cargo workspace root without calling Cargo,
//! - starts a private rust-analyzer LSP process,
//! - configures rust-analyzer to use its server-side file watcher,
//! - responds to the small set of LSP client requests rust-analyzer needs,
//! - injects the patchable-entry compile flags into Cargo,
//! - and, for `cargo run`, builds first and launches the executable itself.
//!
//! This slice proves the service boundary and the first narrow patch RPC:
//! rust-analyzer owns project watching/model state, while `hr` owns Cargo, the
//! target process, and the target-side patch socket.

use std::error::Error;
use std::time::Instant;

#[path = "hr/cargo_driver.rs"]
mod cargo_driver;
#[path = "hr/live.rs"]
mod live;
#[path = "hr/patch/mod.rs"]
mod patch;
#[path = "hr/ra.rs"]
mod ra;
#[path = "hr/rust_source.rs"]
mod rust_source;
#[path = "hr/session.rs"]
mod session;
#[path = "hr/symbols.rs"]
mod symbols;
#[path = "hr/util.rs"]
mod util;

use cargo_driver::run_cargo;
use ra::{maybe_hold_for_project_watch_proof, RustAnalyzerSession};
use session::HotSession;
use util::{find_workspace_root, log_timing};

const PATCHABLE_ENTRY_FLAG: &str = "-Zpatchable-function-entry=16";
const INITIALIZE_ID: i64 = 1;
const SHUTDOWN_ID: i64 = 2;
const WORKSPACE_SYMBOL_ID: i64 = 3;
const VIEW_FILE_TEXT_ID: i64 = 4;
const WATCH_PROOF_ENV: &str = "HR_WATCH_PROOF_SECONDS";
const WATCH_PROOF_SYMBOL_ENV: &str = "HR_WATCH_PROOF_SYMBOL";
const DEFAULT_WATCH_PROOF_SYMBOL: &str = "hot_rust_live_probe";
const LIVE_SYMBOL_ENV: &str = "HR_LIVE_SYMBOL";
const RUNTIME_DYLIB_ENV: &str = "HR_RUNTIME_DYLIB";
const PATCH_BACKEND_ENV: &str = "HR_PATCH_BACKEND";
const CODEGEN_UNITS_ENV: &str = "HR_CODEGEN_UNITS";
const SHADOW_STUBS_ENV: &str = "HR_SHADOW_STUBS";
const SHADOW_PRUNE_ENV: &str = "HR_SHADOW_PRUNE";
const SHADOW_PRESERVE_ENV: &str = "HR_SHADOW_PRESERVE_PREFIXES";
const KEEP_PATCH_ROOT_ENV: &str = "HR_KEEP_PATCH_ROOT";
const PATCH_BUILD_ONLY_ENV: &str = "HR_PATCH_BUILD_ONLY";
const SHADOW_PERSISTENT_ENV: &str = "HR_SHADOW_PERSISTENT";
const TIMING_ENV: &str = "HR_TIMING";

fn main() {
    if let Err(err) = run() {
        eprintln!("hr: error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let total_start = Instant::now();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some((front, cargo_args)) = args.split_first() else {
        usage();
        return Err("missing command; expected `hr cargo <args...>`".into());
    };
    if front != "cargo" {
        usage();
        return Err(format!("unsupported command `{front}`; expected `cargo`").into());
    }
    if cargo_args.is_empty() {
        usage();
        return Err("missing Cargo arguments".into());
    }

    let start = Instant::now();
    let workspace_root = find_workspace_root(&std::env::current_dir()?)?;
    log_timing("workspace-root", start);
    let start = Instant::now();
    let session = HotSession::new(&workspace_root)?;
    log_timing("hot-session", start);
    println!(
        "hr: session {} root {}",
        session.id,
        workspace_root.display()
    );

    let start = Instant::now();
    let ra = RustAnalyzerSession::start(&workspace_root)?;
    log_timing("rust-analyzer-start", start);
    let start = Instant::now();
    let result = run_cargo(&workspace_root, &session, &ra, cargo_args);
    log_timing("cargo-control", start);
    if result.is_ok() {
        maybe_hold_for_project_watch_proof(&ra)?;
    }
    log_timing("total", total_start);
    result
}

fn usage() {
    eprintln!(
        "usage:\n  hr cargo <cargo-args...>\n\nexamples:\n  hr cargo check\n  hr cargo run --bin app -- arg1\n  HR_LIVE_SYMBOL=hot_rust_tick hr cargo run --bin app\n  HR_WATCH_PROOF_SECONDS=30 hr cargo check"
    );
}
