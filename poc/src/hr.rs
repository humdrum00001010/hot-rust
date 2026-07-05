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

use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

fn log_timing(label: &str, start: Instant) {
    if env_flag(TIMING_ENV) {
        println!(
            "hr: timing {label} elapsed={:.3}s",
            start.elapsed().as_secs_f64()
        );
    }
}

#[derive(Debug, Clone)]
struct HotSession {
    id: String,
    socket: PathBuf,
}

impl HotSession {
    fn new(workspace_root: &Path) -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let id = format!("hr-{}-{nonce}", std::process::id());
        let socket = std::env::temp_dir().join(format!("{id}.sock"));

        // Future runtime RPC will bind this path. For M6, removing stale paths
        // catches obvious collisions and makes the env contract deterministic.
        if socket.exists() {
            fs::remove_file(&socket)?;
        }

        println!("hr: hot env prepared before Cargo");
        println!("hr: HR_SOCKET={} (reserved)", socket.display());
        println!("hr: HR_WORKSPACE_ROOT={}", workspace_root.display());

        Ok(Self { id, socket })
    }

    fn apply_env(&self, command: &mut Command, workspace_root: &Path) {
        command
            .env("RUSTC_BOOTSTRAP", "1")
            .env("RUSTFLAGS", merged_rustflags())
            .env("HR_SESSION_ID", &self.id)
            .env("HR_SOCKET", &self.socket)
            .env("HR_WORKSPACE_ROOT", workspace_root);
    }
}

fn run_cargo(
    workspace_root: &Path,
    session: &HotSession,
    ra: &RustAnalyzerSession,
    cargo_args: &[String],
) -> Result<(), Box<dyn Error>> {
    if cargo_args.first().map(String::as_str) == Some("run") {
        return run_cargo_run(workspace_root, session, ra, &cargo_args[1..]);
    }

    println!(
        "hr: cargo {} with {}",
        cargo_args.join(" "),
        PATCHABLE_ENTRY_FLAG
    );
    let mut command = Command::new(cargo_command());
    command.args(cargo_args).current_dir(workspace_root);
    session.apply_env(&mut command, workspace_root);

    let status = command.status()?;
    if !status.success() {
        return Err(format!("cargo exited with {status}").into());
    }

    Ok(())
}

fn run_cargo_run(
    workspace_root: &Path,
    session: &HotSession,
    ra: &RustAnalyzerSession,
    run_args: &[String],
) -> Result<(), Box<dyn Error>> {
    let (cargo_side, binary_args) = split_run_args(run_args);
    if cargo_side
        .iter()
        .any(|arg| arg == "--message-format" || arg.starts_with("--message-format="))
    {
        return Err("hr cargo run reserves --message-format so it can find the executable".into());
    }

    let mut build_args = Vec::with_capacity(cargo_side.len() + 3);
    build_args.push("build".to_string());
    build_args.extend(cargo_side.iter().cloned());
    build_args.push("--message-format=json-render-diagnostics".to_string());
    let bin_name = selected_bin_name(cargo_side);

    println!(
        "hr: translating cargo run -> cargo {}",
        build_args.join(" ")
    );
    let executable = cargo_build_executable(workspace_root, session, &build_args)?;
    println!("hr: executable {}", executable.display());

    let mut child = Command::new(&executable);
    child.args(binary_args).current_dir(workspace_root);
    session.apply_env(&mut child, workspace_root);

    let live = LiveConfig::from_env()?;
    if let Some(live) = live {
        if env_flag(PATCH_BUILD_ONLY_ENV) {
            return build_live_patch_once(
                workspace_root,
                ra,
                &executable,
                live,
                cargo_side,
                bin_name.as_deref(),
            );
        }
        live.apply_runtime_env(&mut child)?;
        println!("hr: launching {}", executable.display());
        let mut child = child.spawn()?;
        return run_live_target(
            workspace_root,
            session,
            ra,
            &executable,
            live,
            cargo_side,
            bin_name.as_deref(),
            &mut child,
        );
    }

    println!("hr: launching {}", executable.display());
    let status = child.status()?;
    if !status.success() {
        return Err(format!("target exited with {status}").into());
    }

    Ok(())
}

fn build_live_patch_once(
    workspace_root: &Path,
    ra: &RustAnalyzerSession,
    executable: &Path,
    live: LiveConfig,
    cargo_side: &[String],
    bin_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let total_start = Instant::now();
    println!(
        "hr: patch build-only mode symbol={} executable={}",
        live.symbol,
        executable.display()
    );
    let start = Instant::now();
    let symbol_resolver = BinarySymbolResolver::load(workspace_root, executable)?;
    log_timing("build-only-symbol-index", start);
    let start = Instant::now();
    let old_runtime_symbol = symbol_resolver.symbol_for(&live.symbol)?;
    log_timing("build-only-symbol-resolve", start);
    println!(
        "hr: live runtime symbol {} -> {}",
        live.symbol, old_runtime_symbol
    );
    let module_hint = source_path_hint_from_symbol(&old_runtime_symbol, &live.symbol);
    if let Some(module_hint) = &module_hint {
        println!("hr: live source module hint {}", module_hint.join("::"));
    }
    let start = Instant::now();
    let source_uri = discover_live_source_uri(
        workspace_root,
        ra,
        &live.symbol,
        bin_name,
        module_hint.as_deref(),
    )?;
    log_timing("build-only-source-discovery", start);
    println!("hr: live source symbol {} uri {}", live.symbol, source_uri);

    let start = Instant::now();
    let source_text = source_text_from_ra_or_disk(ra, &source_uri)?;
    log_timing("build-only-source-text", start);
    println!("hr: live source text bytes={}", source_text.len());
    let start = Instant::now();
    let function = extract_function(&source_text, &live.symbol).ok_or_else(|| {
        format!(
            "could not parse function {}; snippet={}",
            live.symbol,
            source_snippet(&source_text, &live.symbol)
        )
    })?;
    log_timing("build-only-source-parse", start);
    println!(
        "hr: patch build-only {} signature `{}` body-bytes={}",
        live.symbol,
        function.signature.trim(),
        function.body.len()
    );
    let start = Instant::now();
    let _patch = build_function_patch_dylib(
        workspace_root,
        executable,
        cargo_side,
        &source_uri,
        &old_runtime_symbol,
        &live.symbol,
        &live.patch_symbol,
        &function,
        Some(&symbol_resolver),
    )?;
    log_timing("build-only-patch-build", start);
    println!("hr: patch build-only completed");
    log_timing("build-only-total", total_start);
    Ok(())
}

fn cargo_build_executable(
    workspace_root: &Path,
    session: &HotSession,
    build_args: &[String],
) -> Result<PathBuf, Box<dyn Error>> {
    let start = Instant::now();
    let mut command = Command::new(cargo_command());
    command
        .args(build_args)
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    session.apply_env(&mut command, workspace_root);

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or("failed to capture cargo JSON stdout")?;
    let reader = BufReader::new(stdout);
    let mut executable = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(value) => {
                if let Some(rendered) = value
                    .get("message")
                    .and_then(|message| message.get("rendered"))
                    .and_then(Value::as_str)
                {
                    eprint!("{rendered}");
                }

                if value.get("reason").and_then(Value::as_str) == Some("compiler-artifact") {
                    if let Some(path) = value.get("executable").and_then(Value::as_str) {
                        executable = Some(PathBuf::from(path));
                    }
                }
            }
            Err(_) => println!("{line}"),
        }
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("cargo build exited with {status}").into());
    }
    log_timing("target-cargo-build", start);

    executable.ok_or_else(|| "cargo build did not report an executable artifact".into())
}

struct LiveConfig {
    symbol: String,
    patch_symbol: String,
    runtime_dylib: PathBuf,
}

impl LiveConfig {
    fn from_env() -> Result<Option<Self>, Box<dyn Error>> {
        let Some(symbol) = std::env::var_os(LIVE_SYMBOL_ENV) else {
            return Ok(None);
        };
        let symbol = symbol.to_string_lossy().into_owned();
        let runtime_dylib = std::env::var_os(RUNTIME_DYLIB_ENV)
            .map(PathBuf::from)
            .or_else(default_runtime_dylib);
        let runtime_dylib = runtime_dylib.ok_or_else(|| {
            format!("{RUNTIME_DYLIB_ENV} is required for {LIVE_SYMBOL_ENV}={symbol}")
        })?;
        if !runtime_dylib.is_file() {
            return Err(format!("runtime dylib not found: {}", runtime_dylib.display()).into());
        }

        Ok(Some(Self {
            patch_symbol: format!("hot_rust_patch_{symbol}"),
            symbol,
            runtime_dylib,
        }))
    }

    fn apply_runtime_env(&self, command: &mut Command) -> Result<(), Box<dyn Error>> {
        let mut libraries = OsString::from(&self.runtime_dylib);
        if let Some(existing) = std::env::var_os("DYLD_INSERT_LIBRARIES") {
            libraries.push(":");
            libraries.push(existing);
        }
        command.env("DYLD_INSERT_LIBRARIES", libraries);
        Ok(())
    }
}

fn default_runtime_dylib() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    #[cfg(target_os = "macos")]
    let name = "libhr_runtime.dylib";
    #[cfg(target_os = "linux")]
    let name = "libhr_runtime.so";
    #[cfg(target_os = "windows")]
    let name = "hr_runtime.dll";
    Some(dir.join(name))
}

fn run_live_target(
    workspace_root: &Path,
    session: &HotSession,
    ra: &RustAnalyzerSession,
    executable: &Path,
    live: LiveConfig,
    cargo_side: &[String],
    bin_name: Option<&str>,
    child: &mut Child,
) -> Result<(), Box<dyn Error>> {
    println!(
        "hr: live mode symbol={} runtime={}",
        live.symbol,
        live.runtime_dylib.display()
    );
    wait_for_socket(&session.socket, Duration::from_secs(10))?;

    let start = Instant::now();
    let symbol_resolver = BinarySymbolResolver::load(workspace_root, executable)?;
    log_timing("live-symbol-index", start);
    let start = Instant::now();
    let old_runtime_symbol = symbol_resolver.symbol_for(&live.symbol)?;
    log_timing("live-symbol-resolve", start);
    println!(
        "hr: live runtime symbol {} -> {}",
        live.symbol, old_runtime_symbol
    );
    let module_hint = source_path_hint_from_symbol(&old_runtime_symbol, &live.symbol);
    if let Some(module_hint) = &module_hint {
        println!("hr: live source module hint {}", module_hint.join("::"));
    }
    let source_uri = discover_live_source_uri(
        workspace_root,
        ra,
        &live.symbol,
        bin_name,
        module_hint.as_deref(),
    )?;
    println!("hr: live source symbol {} uri {}", live.symbol, source_uri);

    let mut source_text = source_text_from_ra_or_disk(ra, &source_uri)?;
    println!("hr: live source text bytes={}", source_text.len());
    let mut current_function = extract_function(&source_text, &live.symbol).ok_or_else(|| {
        format!(
            "could not parse function {}; snippet={}",
            live.symbol,
            source_snippet(&source_text, &live.symbol)
        )
    })?;
    let required_signature = current_function.signature.clone();
    println!(
        "hr: live initial {} signature `{}` body-bytes={}",
        live.symbol,
        required_signature.trim(),
        current_function.body.len()
    );

    let mut patches = Vec::new();
    let mut activity_baseline = ra.activity_seq();
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(format!("target exited with {status}").into());
            }
            return Ok(());
        }

        let reason = if let Some(reason) =
            ra.wait_for_activity_after(activity_baseline, Duration::from_millis(500))?
        {
            activity_baseline = ra.activity_seq();
            reason
        } else {
            let _ = ra.workspace_symbol_contains(&live.symbol)?;
            "workspace/symbol refresh".to_string()
        };

        let next_text = source_text_from_ra_or_disk(ra, &source_uri)?;
        if next_text == source_text {
            continue;
        }
        println!("hr: live source check after rust-analyzer {reason}");
        source_text = next_text;

        let Some(next_function) = extract_function(&source_text, &live.symbol) else {
            println!(
                "hr: live edit seen but {} is not parseable yet; snippet={}",
                live.symbol,
                source_snippet(&source_text, &live.symbol)
            );
            continue;
        };
        if next_function.signature != required_signature {
            println!(
                "hr: live edit seen but {} signature changed; rebuild required. old=`{}` new=`{}`",
                live.symbol,
                required_signature.trim(),
                next_function.signature.trim()
            );
            continue;
        }
        if next_function.body == current_function.body {
            continue;
        }

        println!(
            "hr: live source edit {} body bytes {} -> {}",
            live.symbol,
            current_function.body.len(),
            next_function.body.len()
        );
        if PatchBackend::from_env() == PatchBackend::CguOnly {
            let probe =
                build_incremental_cgu_probe(workspace_root, cargo_side, &old_runtime_symbol)?;
            probe.report();
            let object = probe
                .after
                .as_ref()
                .ok_or("dirty-CGU object patch requested but no updated object was found")?;
            send_object_patch_command(session, &old_runtime_symbol, &object.path)?;
            current_function = next_function;
            continue;
        }
        let patch = build_function_patch_dylib(
            workspace_root,
            executable,
            cargo_side,
            &source_uri,
            &old_runtime_symbol,
            &live.symbol,
            &live.patch_symbol,
            &next_function,
            Some(&symbol_resolver),
        )?;
        send_patch_command(session, &old_runtime_symbol, &live, &patch)?;
        current_function = next_function;
        patches.push(patch);
    }
}

fn source_snippet(source: &str, symbol: &str) -> String {
    let Some(pos) = source.find(symbol) else {
        return "<symbol not found>".to_string();
    };
    let start = pos.saturating_sub(80);
    let end = (pos + 160).min(source.len());
    source[start..end].replace('\n', "\\n")
}

fn source_text_from_ra_or_disk(
    ra: &RustAnalyzerSession,
    uri: &str,
) -> Result<String, Box<dyn Error>> {
    let text = ra.view_file_text(uri)?;
    if !text.is_empty() {
        return Ok(text);
    }

    let path = file_uri_to_path(uri)?;
    Ok(fs::read_to_string(path)?)
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf, Box<dyn Error>> {
    let raw = uri
        .strip_prefix("file://")
        .ok_or_else(|| format!("not a file URI: {uri}"))?;
    let mut bytes = Vec::with_capacity(raw.len());
    let raw = raw.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let hex =
                std::str::from_utf8(raw.get(index + 1..index + 3).ok_or("short URI escape")?)?;
            bytes.push(u8::from_str_radix(hex, 16)?);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(String::from_utf8(bytes)?))
}

fn wait_for_socket(socket: &Path, duration: Duration) -> Result<(), Box<dyn Error>> {
    let deadline = SystemTime::now() + duration;
    loop {
        if socket.exists() {
            return Ok(());
        }
        if SystemTime::now() >= deadline {
            return Err(format!("runtime socket not ready at {}", socket.display()).into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn send_patch_command(
    session: &HotSession,
    old_runtime_symbol: &str,
    live: &LiveConfig,
    patch: &BuiltLivePatch,
) -> Result<(), Box<dyn Error>> {
    let mut stream = std::os::unix::net::UnixStream::connect(&session.socket)?;
    let stubs = patch
        .stubs
        .iter()
        .map(|stub| {
            json!({
                "source_symbol": stub.source_symbol,
                "stub_symbol": stub.stub_symbol,
                "old_symbol": stub.old_symbol,
            })
        })
        .collect::<Vec<_>>();
    writeln!(
        stream,
        "{}",
        json!({
            "old_symbol": old_runtime_symbol,
            "patch_dylib": patch.dylib,
            "new_symbol": live.patch_symbol,
            "stubs": stubs,
        })
    )?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if !response.starts_with("OK ") {
        return Err(format!("runtime patch failed: {response}").into());
    }
    println!("hr: runtime patch {}", response.trim());
    Ok(())
}

fn send_object_patch_command(
    session: &HotSession,
    old_runtime_symbol: &str,
    object_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut stream = std::os::unix::net::UnixStream::connect(&session.socket)?;
    writeln!(
        stream,
        "{}",
        json!({
            "old_symbol": old_runtime_symbol,
            "object_path": object_path.display().to_string(),
            "new_symbol": old_runtime_symbol,
        })
    )?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    if !response.starts_with("OK ") {
        return Err(format!("runtime object patch failed: {response}").into());
    }
    println!("hr: runtime object patch {}", response.trim());
    Ok(())
}

struct BinarySymbolResolver {
    executable: PathBuf,
    candidates: Vec<SymbolCandidate>,
}

impl BinarySymbolResolver {
    fn load(workspace_root: &Path, executable: &Path) -> Result<Self, Box<dyn Error>> {
        let output = Command::new("nm").arg(executable).output()?;
        if !output.status.success() {
            return Err(format!("nm exited with {}", output.status).into());
        }
        let workspace_crates = workspace_crate_names(workspace_root)?;
        let stdout = String::from_utf8(output.stdout)?;
        let mut candidates = Vec::new();
        for line in stdout.lines() {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 3 {
                continue;
            }
            let kind = fields[fields.len() - 2];
            if kind != "t" && kind != "T" {
                continue;
            }
            let symbol = fields[fields.len() - 1];
            if symbol.contains("closure") {
                continue;
            }
            let stripped = symbol.strip_prefix('_').unwrap_or(symbol).to_string();
            let components = rust_mangled_symbol_components(&stripped);
            let crate_match = components
                .iter()
                .any(|component| workspace_crates.iter().any(|name| name == component));
            candidates.push(SymbolCandidate {
                symbol: stripped,
                components,
                crate_match,
            });
        }
        Ok(Self {
            executable: executable.to_path_buf(),
            candidates,
        })
    }

    fn symbol_for(&self, source_symbol: &str) -> Result<String, Box<dyn Error>> {
        let mut candidates = self
            .candidates
            .iter()
            .filter(|candidate| candidate.symbol.contains(source_symbol))
            .map(|candidate| {
                let exact_component = candidate
                    .components
                    .iter()
                    .any(|component| component == source_symbol);
                (candidate, exact_component)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(candidate, exact_component)| {
            (
                !candidate.crate_match,
                !*exact_component,
                candidate.symbol.len(),
                candidate.symbol.clone(),
            )
        });
        let Some((candidate, _)) = candidates.first() else {
            return Err(format!(
                "could not find text symbol containing `{source_symbol}` in {}",
                self.executable.display()
            )
            .into());
        };
        Ok(candidate.symbol.clone())
    }
}

struct SymbolCandidate {
    symbol: String,
    components: Vec<String>,
    crate_match: bool,
}

fn workspace_crate_names(workspace_root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let mut command = Command::new(cargo_command());
    command
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env_remove("DYLD_INSERT_LIBRARIES")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!("cargo metadata exited with {}", output.status).into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata missing packages")?;
    let mut names = packages
        .iter()
        .filter_map(|package| package.get("name").and_then(Value::as_str))
        .map(|name| name.replace('-', "_"))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

fn source_path_hint_from_symbol(runtime_symbol: &str, source_symbol: &str) -> Option<Vec<String>> {
    let components = rust_mangled_symbol_components(runtime_symbol);
    let symbol_index = components
        .iter()
        .rposition(|component| component == source_symbol)?;
    if symbol_index <= 1 {
        return None;
    }
    let path_components = &components[..symbol_index];
    let crate_index = path_components
        .iter()
        .position(|component| is_probable_rust_path_ident(component))?;
    let modules = path_components[crate_index + 1..].to_vec();
    (!modules.is_empty()).then_some(modules)
}

fn is_probable_rust_path_ident(component: &str) -> bool {
    let mut chars = component.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_lowercase())
        && chars.all(|ch| ch == '_' || ch.is_ascii_lowercase() || ch.is_ascii_digit())
}

fn rust_mangled_symbol_components(symbol: &str) -> Vec<String> {
    let bytes = symbol.as_bytes();
    let mut components = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        if index > 0 && bytes[index - 1] == b'B' {
            while index < bytes.len() && bytes[index] != b'_' {
                index += 1;
            }
            if index < bytes.len() {
                index += 1;
            }
            continue;
        }

        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }

        let len_start = index;
        let mut len = 0usize;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            len = len
                .saturating_mul(10)
                .saturating_add((bytes[index] - b'0') as usize);
            index += 1;
        }

        if len == 0 || index + len > bytes.len() {
            index = len_start + 1;
            continue;
        }

        let raw = &bytes[index..index + len];
        if raw
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            if let Ok(component) = std::str::from_utf8(raw) {
                components.push(component.to_string());
            }
            index += len;
        } else {
            index = len_start + 1;
        }
    }

    components
}

struct BuiltLivePatch {
    root: PathBuf,
    dylib: PathBuf,
    stubs: Vec<PatchStub>,
    cleanup_root: bool,
}

#[derive(Debug, Clone)]
struct PatchStub {
    source_symbol: String,
    stub_symbol: String,
    old_symbol: String,
}

impl Drop for BuiltLivePatch {
    fn drop(&mut self) {
        if env_flag(KEEP_PATCH_ROOT_ENV) {
            println!("hr: keeping patch root {}", self.root.display());
            return;
        }
        if self.cleanup_root {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchBackend {
    Dylib,
    ShadowStub,
    ShadowMini,
    ShadowFake,
    ObjectProbe,
    ObjectOnly,
    CguProbe,
    CguOnly,
}

impl PatchBackend {
    fn from_env() -> Self {
        match std::env::var(PATCH_BACKEND_ENV) {
            Ok(value) if value == "shadow-stub" || value == "shadow-stubs" => Self::ShadowStub,
            Ok(value)
                if value == "shadow-mini"
                    || value == "shadow-stub-mini"
                    || value == "shadow-ministub" =>
            {
                Self::ShadowMini
            }
            Ok(value)
                if value == "shadow-fake"
                    || value == "fake-crate"
                    || value == "shadow-directive" =>
            {
                Self::ShadowFake
            }
            Ok(value) if value == "object-probe" => Self::ObjectProbe,
            Ok(value) if value == "object" => Self::ObjectOnly,
            Ok(value) if value == "cgu-probe" => Self::CguProbe,
            Ok(value) if value == "cgu" || value == "cgu-only" => Self::CguOnly,
            Ok(value) if !value.is_empty() => {
                println!(
                    "hr: ignoring unsupported {PATCH_BACKEND_ENV}={value}; using dylib backend"
                );
                Self::Dylib
            }
            _ => Self::Dylib,
        }
    }

    fn wants_object_probe(self) -> bool {
        matches!(self, Self::ObjectProbe | Self::ObjectOnly)
    }

    fn wants_cgu_probe(self) -> bool {
        matches!(self, Self::CguProbe | Self::CguOnly)
    }
}

fn build_function_patch_dylib(
    workspace_root: &Path,
    executable: &Path,
    cargo_side: &[String],
    source_uri: &str,
    runtime_symbol: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
    symbol_resolver: Option<&BinarySymbolResolver>,
) -> Result<BuiltLivePatch, Box<dyn Error>> {
    let backend = PatchBackend::from_env();
    if backend.wants_cgu_probe() {
        match build_incremental_cgu_probe(workspace_root, cargo_side, runtime_symbol) {
            Ok(probe) => probe.report(),
            Err(err) => println!("hr: dirty-CGU probe failed before no-link evidence: {err}"),
        }
        if backend == PatchBackend::CguOnly {
            return Err(
                "HR_PATCH_BACKEND=cgu currently probes real incremental object emission only; runtime object relocation is not installed yet"
                    .into(),
            );
        }
    }

    if backend.wants_object_probe() {
        match build_function_patch_object_probe(
            workspace_root,
            source_uri,
            old_symbol,
            patch_symbol,
            function,
        ) {
            Ok(probe) => probe.report(),
            Err(err) => println!("hr: exact-fn object probe failed before object emission: {err}"),
        }
        if backend == PatchBackend::ObjectOnly {
            return Err(
                "HR_PATCH_BACKEND=object currently probes object emission only; runtime object relocation is not installed yet"
                    .into(),
            );
        }
    }

    if method_receiver(&function.signature).is_some() {
        if matches!(
            backend,
            PatchBackend::ShadowStub | PatchBackend::ShadowMini | PatchBackend::ShadowFake
        ) {
            return build_shadow_stub_patch_dylib(
                workspace_root,
                executable,
                source_uri,
                old_symbol,
                patch_symbol,
                function,
                backend == PatchBackend::ShadowMini
                    || backend == PatchBackend::ShadowFake
                    || env_flag(SHADOW_PRUNE_ENV),
                backend == PatchBackend::ShadowFake,
                symbol_resolver,
            );
        }
        return build_shadow_crate_patch_dylib(
            workspace_root,
            source_uri,
            old_symbol,
            patch_symbol,
            function,
        );
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "hot-rust-live-patch-{}-{nonce}",
        std::process::id()
    ));
    let src = root.join("src");
    fs::create_dir_all(&src)?;
    let root = root.canonicalize()?;
    let src = root.join("src");
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "hot-rust-live-patch"
version = "0.1.0"
edition = "2021"

[lib]
name = "hot_rust_live_patch"
crate-type = ["cdylib"]
"#,
    )?;
    let patch_signature = patch_signature(old_symbol, patch_symbol, &function.signature)?;
    fs::write(
        src.join("lib.rs"),
        format!(
            "#[no_mangle]\n#[inline(never)]\n{} {{\n{}\n}}\n",
            patch_signature.trim(),
            function.body
        ),
    )?;

    let mut command = Command::new(cargo_command());
    command
        .arg("build")
        .current_dir(&root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("DYLD_INSERT_LIBRARIES");
    let status = command.status()?;
    if !status.success() {
        return Err(format!("patch cargo build exited with {status}").into());
    }

    let dylib = root
        .join("target")
        .join("debug")
        .join(dylib_filename("hot_rust_live_patch"));
    if !dylib.is_file() {
        return Err(format!("patch dylib missing: {}", dylib.display()).into());
    }
    Ok(BuiltLivePatch {
        root,
        dylib,
        stubs: Vec::new(),
        cleanup_root: true,
    })
}

struct ObjectProbe {
    root: PathBuf,
    object: Option<PathBuf>,
    elapsed: Duration,
    status: String,
    symbols: Vec<String>,
    relocations: Vec<String>,
    compiler_notes: Vec<String>,
}

impl ObjectProbe {
    fn report(&self) {
        println!(
            "hr: exact-fn object probe status={} elapsed={:.2}s root={}",
            self.status,
            self.elapsed.as_secs_f64(),
            self.root.display()
        );
        if let Some(object) = &self.object {
            println!("hr: exact-fn object {}", object.display());
        }
        if !self.symbols.is_empty() {
            println!("hr: exact-fn object symbols:");
            for line in &self.symbols {
                println!("hr:   {line}");
            }
        }
        if !self.relocations.is_empty() {
            println!("hr: exact-fn object relocations:");
            for line in &self.relocations {
                println!("hr:   {line}");
            }
        }
        if !self.compiler_notes.is_empty() {
            println!("hr: exact-fn object compiler notes:");
            for line in &self.compiler_notes {
                println!("hr:   {line}");
            }
        }
    }
}

impl Drop for ObjectProbe {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct CguProbe {
    target_dir: PathBuf,
    before: Option<CguObject>,
    after: Option<CguObject>,
    elapsed: Duration,
    status: String,
    compiler_notes: Vec<String>,
}

impl CguProbe {
    fn report(&self) {
        println!(
            "hr: dirty-CGU probe status={} elapsed={:.2}s target={}",
            self.status,
            self.elapsed.as_secs_f64(),
            self.target_dir.display()
        );
        match &self.before {
            Some(object) => println!("hr: dirty-CGU before {}", object.summary()),
            None => println!("hr: dirty-CGU before <not found>"),
        }
        match &self.after {
            Some(object) => println!("hr: dirty-CGU after  {}", object.summary()),
            None => println!("hr: dirty-CGU after  <not found>"),
        }
        if !self.compiler_notes.is_empty() {
            println!("hr: dirty-CGU compiler notes:");
            for line in &self.compiler_notes {
                println!("hr:   {line}");
            }
        }
    }
}

#[derive(Clone)]
struct CguObject {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    sha256: Option<String>,
}

impl CguObject {
    fn summary(&self) -> String {
        format!(
            "path={} bytes={} mtime={} sha256={}",
            self.path.display(),
            self.len,
            self.modified
                .map(format_system_time)
                .unwrap_or_else(|| "unknown".to_string()),
            self.sha256.as_deref().unwrap_or("unknown")
        )
    }

    fn same_bits(&self, other: &Self) -> bool {
        self.path == other.path && self.len == other.len && self.sha256 == other.sha256
    }
}

fn build_incremental_cgu_probe(
    workspace_root: &Path,
    cargo_side: &[String],
    runtime_symbol: &str,
) -> Result<CguProbe, Box<dyn Error>> {
    let target_dir = cargo_target_dir(workspace_root)?;
    println!(
        "hr: dirty-CGU searching target objects for {} below {}",
        runtime_symbol,
        target_dir.display()
    );
    let before = find_cgu_object_defining_symbol(&target_dir, runtime_symbol)?;

    let mut command = Command::new(cargo_command());
    command
        .arg("rustc")
        .args(cargo_side)
        .args(["--", "-Z", "no-link"])
        .current_dir(workspace_root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env_remove("DYLD_INSERT_LIBRARIES");

    println!(
        "hr: dirty-CGU running cargo rustc {} -- -Z no-link",
        cargo_side.join(" ")
    );
    let start = Instant::now();
    let output = command.output()?;
    let elapsed = start.elapsed();
    let compiler_notes = compiler_notes(&output.stdout, &output.stderr);
    let after = find_cgu_object_defining_symbol(&target_dir, runtime_symbol)?;

    let status = if output.status.success() {
        match (&before, &after) {
            (Some(before), Some(after)) if before.same_bits(after) => "unchanged".to_string(),
            (Some(_), Some(_)) => "dirty-object-updated".to_string(),
            (None, Some(_)) => "dirty-object-created".to_string(),
            (Some(_), None) => "object-lost".to_string(),
            (None, None) => "object-not-found".to_string(),
        }
    } else {
        format!("compile-failed({})", output.status)
    };

    Ok(CguProbe {
        target_dir,
        before,
        after,
        elapsed,
        status,
        compiler_notes,
    })
}

fn cargo_target_dir(workspace_root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(raw) = std::env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(raw);
        return Ok(if path.is_absolute() {
            path
        } else {
            workspace_root.join(path)
        });
    }

    let output = Command::new(cargo_command())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env_remove("DYLD_INSERT_LIBRARIES")
        .output()?;
    if !output.status.success() {
        return Err(format!("cargo metadata exited with {}", output.status).into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    metadata
        .get("target_directory")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata missing target_directory".into())
}

fn find_cgu_object_defining_symbol(
    target_dir: &Path,
    runtime_symbol: &str,
) -> Result<Option<CguObject>, Box<dyn Error>> {
    let spellings = macho_symbol_spellings(runtime_symbol);
    let mut objects = Vec::new();
    for path in object_paths_containing_any(target_dir, &spellings) {
        if object_defines_text_symbol(&path, &spellings) {
            objects.push(cgu_object(&path)?);
        }
    }
    objects.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| right.path.cmp(&left.path))
    });
    Ok(objects.into_iter().next())
}

fn object_paths_containing_any(root: &Path, needles: &[String]) -> Vec<PathBuf> {
    let fast_paths = rg_object_paths_containing_any(root, needles);
    if !fast_paths.is_empty() {
        return fast_paths;
    }

    let mut paths = Vec::new();
    let mut stack = object_search_roots(root);
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("o") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if needles
                .iter()
                .any(|needle| contains_bytes(&bytes, needle.as_bytes()))
            {
                paths.push(path);
            }
        }
    }
    paths
}

fn rg_object_paths_containing_any(root: &Path, needles: &[String]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for search_root in object_search_roots(root) {
        for needle in needles {
            let Ok(output) = Command::new("rg")
                .args(["-a", "-l", "-F", "-g", "*.o", "--"])
                .arg(needle)
                .arg(&search_root)
                .output()
            else {
                continue;
            };
            if !output.status.success() && output.status.code() != Some(1) {
                continue;
            }
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let path = PathBuf::from(line.trim());
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn object_search_roots(target_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for profile in ["debug", "release"] {
        let incremental = target_dir.join(profile).join("incremental");
        if incremental.is_dir() {
            roots.push(incremental);
        }
    }
    if roots.is_empty() {
        roots.push(target_dir.to_path_buf());
    }
    roots
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn object_defines_text_symbol(path: &Path, spellings: &[String]) -> bool {
    let Ok(output) = Command::new("nm").arg("-m").arg(path).output() else {
        return false;
    };
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.lines().any(|line| {
        if !line.contains("(__TEXT,__text)") {
            return false;
        }
        let Some(symbol) = line.split_whitespace().last() else {
            return false;
        };
        spellings.iter().any(|spelling| symbol == spelling)
    })
}

fn cgu_object(path: &Path) -> io::Result<CguObject> {
    let metadata = fs::metadata(path)?;
    Ok(CguObject {
        path: path.to_path_buf(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
        sha256: file_sha256(path),
    })
}

fn macho_symbol_spellings(runtime_symbol: &str) -> Vec<String> {
    let mut spellings = vec![runtime_symbol.to_string()];
    if !runtime_symbol.starts_with('_') {
        spellings.push(format!("_{runtime_symbol}"));
    } else {
        spellings.push(format!("_{runtime_symbol}"));
        spellings.push(runtime_symbol.trim_start_matches('_').to_string());
    }
    spellings.sort();
    spellings.dedup();
    spellings
}

fn file_sha256(path: &Path) -> Option<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(ToString::to_string)
}

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:09}", duration.as_secs(), duration.subsec_nanos()),
        Err(_) => "before-unix-epoch".to_string(),
    }
}

fn build_function_patch_object_probe(
    workspace_root: &Path,
    source_uri: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
) -> Result<ObjectProbe, Box<dyn Error>> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "hot-rust-object-probe-{}-{nonce}",
        std::process::id()
    ));
    let src = root.join("src");
    fs::create_dir_all(&src)?;
    let root = root.canonicalize()?;
    let src = root.join("src");

    let source = if method_receiver(&function.signature).is_some() {
        method_object_probe_source(
            workspace_root,
            source_uri,
            old_symbol,
            patch_symbol,
            function,
        )?
    } else {
        let patch_signature = patch_signature(old_symbol, patch_symbol, &function.signature)?;
        ObjectProbeSource {
            manifest: object_probe_manifest(None),
            source: format!(
                "#[no_mangle]\n#[inline(never)]\n{} {{\n{}\n}}\n",
                patch_signature.trim(),
                function.body
            ),
        }
    };
    fs::write(root.join("Cargo.toml"), source.manifest)?;
    fs::write(src.join("lib.rs"), source.source)?;

    let target_dir = root.join("target");
    let start = Instant::now();
    let output = Command::new(cargo_command())
        .args(["rustc", "--lib", "--", "--emit=obj"])
        .current_dir(&root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env_remove("DYLD_INSERT_LIBRARIES")
        .output()?;
    let elapsed = start.elapsed();
    let compiler_notes = compiler_notes(&output.stdout, &output.stderr);

    if !output.status.success() {
        return Ok(ObjectProbe {
            root,
            object: None,
            elapsed,
            status: format!("compile-failed({})", output.status),
            symbols: Vec::new(),
            relocations: Vec::new(),
            compiler_notes,
        });
    }

    let object = newest_file_with_extension(&target_dir, "o")
        .ok_or_else(|| format!("no object emitted below {}", target_dir.display()))?;
    let symbols = command_lines(Command::new("nm").arg("-m").arg(&object), 24);
    let relocations = command_lines(Command::new("otool").arg("-r").arg(&object), 48);

    Ok(ObjectProbe {
        root,
        object: Some(object),
        elapsed,
        status: "emitted".to_string(),
        symbols,
        relocations,
        compiler_notes,
    })
}

struct ObjectProbeSource {
    manifest: String,
    source: String,
}

fn object_probe_manifest(workspace_package: Option<(&str, &Path)>) -> String {
    let mut manifest = String::from(
        r#"[package]
name = "hot-rust-object-probe"
version = "0.1.0"
edition = "2021"

[lib]
name = "hot_rust_object_probe"
crate-type = ["rlib"]
"#,
    );
    if let Some((package, path)) = workspace_package {
        manifest.push_str("\n[dependencies]\n");
        manifest.push_str(&format!("{package} = {{ path = {:?} }}\n", path));
    }
    manifest
}

fn method_object_probe_source(
    workspace_root: &Path,
    source_uri: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
) -> Result<ObjectProbeSource, Box<dyn Error>> {
    let source_path = file_uri_to_path(source_uri)?;
    let source_text = fs::read_to_string(&source_path)?;
    let impl_type =
        containing_impl_type(&source_text, function.signature_start).ok_or_else(|| {
            format!(
                "could not find containing impl type for method object probe `{old_symbol}` in {}",
                source_path.display()
            )
        })?;
    let package = workspace_root_package_name(workspace_root)?;
    let module_path = module_path_from_source_path(workspace_root, &source_path)?;
    let impl_path = format!("{package}::{}::{impl_type}", module_path.join("::"));
    let imports = method_probe_imports(&package, &module_path);
    let signature = method_object_probe_signature(patch_symbol, &impl_path, &function.signature)?;
    let body = method_probe_body(&function.body, &impl_type);

    Ok(ObjectProbeSource {
        manifest: object_probe_manifest(Some((&package, workspace_root))),
        source: format!(
            "extern crate {package};\n{imports}\n#[no_mangle]\n#[inline(never)]\n{signature} {{\n{body}\n}}\n"
        ),
    })
}

fn method_probe_imports(package: &str, module_path: &[String]) -> String {
    let mut imports = Vec::new();
    imports.push(format!("use {package}::{}::*;", module_path.join("::")));
    if module_path.len() >= 2 && module_path[0] == "renderer" {
        imports.push(format!("use {package}::renderer::render_tree::*;"));
        imports.push(format!("use {package}::renderer::*;"));
    }
    imports.join("\n")
}

fn method_object_probe_signature(
    patch_symbol: &str,
    impl_path: &str,
    signature: &str,
) -> Result<String, Box<dyn Error>> {
    let open = signature.find('(').ok_or("method signature has no `(`")?;
    let close = matching_paren(signature, open).ok_or("method signature has no matching `)`")?;
    let params = split_top_level_commas(&signature[open + 1..close]);
    let Some(receiver) = params.first().map(|param| param.trim()) else {
        return Err("method signature has no receiver".into());
    };
    let this_param = match receiver
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .as_str()
    {
        "&mut self" => format!("this: &mut {impl_path}"),
        "&self" => format!("this: &{impl_path}"),
        "self" => format!("this: {impl_path}"),
        "mut self" => format!("mut this: {impl_path}"),
        other => return Err(format!("unsupported method receiver `{other}`").into()),
    };

    let mut wrapper_params = vec![this_param];
    wrapper_params.extend(
        params
            .iter()
            .skip(1)
            .map(|param| param.trim())
            .filter(|param| !param.is_empty())
            .map(ToString::to_string),
    );

    let tail = signature[close + 1..].trim();
    let tail = if tail.is_empty() {
        String::new()
    } else {
        format!(" {tail}")
    };
    Ok(format!(
        "pub fn {patch_symbol}({}){tail}",
        wrapper_params.join(", ")
    ))
}

fn method_probe_body(body: &str, impl_type: &str) -> String {
    body.replace("self.", "this.")
        .replace("self\n", "this\n")
        .replace("Self::", &format!("{impl_type}::"))
}

fn module_path_from_source_path(
    workspace_root: &Path,
    source_path: &Path,
) -> Result<Vec<String>, Box<dyn Error>> {
    let relative = source_path.strip_prefix(workspace_root)?;
    let mut components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if components.first().map(String::as_str) == Some("src") {
        components.remove(0);
    }
    if let Some(last) = components.last_mut() {
        if last == "mod.rs" {
            components.pop();
        } else if let Some(stem) = last.strip_suffix(".rs") {
            *last = stem.to_string();
        }
    }
    if components.is_empty() || components == ["lib"] || components == ["main"] {
        return Err(format!("could not derive module path for {}", source_path.display()).into());
    }
    Ok(components)
}

fn workspace_root_package_name(workspace_root: &Path) -> Result<String, Box<dyn Error>> {
    let mut command = Command::new(cargo_command());
    command
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env_remove("DYLD_INSERT_LIBRARIES")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!("cargo metadata exited with {}", output.status).into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let root_id = metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("root"))
        .and_then(Value::as_str);
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata missing packages")?;
    if let Some(root_id) = root_id {
        if let Some(name) = packages.iter().find_map(|package| {
            (package.get("id").and_then(Value::as_str) == Some(root_id))
                .then(|| package.get("name").and_then(Value::as_str))
                .flatten()
        }) {
            return Ok(name.replace('-', "_"));
        }
    }
    packages
        .first()
        .and_then(|package| package.get("name").and_then(Value::as_str))
        .map(|name| name.replace('-', "_"))
        .ok_or_else(|| "cargo metadata did not report a root package".into())
}

fn newest_file_with_extension(root: &Path, extension: &str) -> Option<PathBuf> {
    let mut newest = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some(extension)
            {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok();
                if newest
                    .as_ref()
                    .and_then(|(_, time): &(PathBuf, Option<SystemTime>)| *time)
                    .map(|time| modified.map(|modified| modified > time).unwrap_or(false))
                    .unwrap_or(true)
                {
                    newest = Some((path, modified));
                }
            }
        }
    }
    newest.map(|(path, _)| path)
}

fn command_lines(command: &mut Command, limit: usize) -> Vec<String> {
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(limit)
        .map(ToString::to_string)
        .collect()
}

fn compiler_notes(stdout: &[u8], stderr: &[u8]) -> Vec<String> {
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(stdout));
    text.push_str(&String::from_utf8_lossy(stderr));
    let mut notes = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with("error")
            || line.starts_with("warning")
            || line.starts_with("note")
            || line.starts_with("-->")
            || line.contains("could not compile")
        {
            notes.push(line.to_string());
            if notes.len() >= 30 {
                break;
            }
        }
    }
    if notes.is_empty() {
        notes.extend(
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(12)
                .map(ToString::to_string),
        );
    }
    notes
}

fn build_shadow_crate_patch_dylib(
    workspace_root: &Path,
    source_uri: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
) -> Result<BuiltLivePatch, Box<dyn Error>> {
    let source_path = file_uri_to_path(source_uri)?;
    let relative_source = source_path.strip_prefix(workspace_root)?;
    let source_text = fs::read_to_string(&source_path)?;
    let impl_type =
        containing_impl_type(&source_text, function.signature_start).ok_or_else(|| {
            format!(
                "could not find containing impl type for method `{old_symbol}` in {}",
                source_path.display()
            )
        })?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "hot-rust-shadow-patch-{}-{nonce}",
        std::process::id()
    ));
    copy_workspace_for_shadow(workspace_root, &root)?;
    let root = root.canonicalize()?;

    let lib_name = format!("hot_rust_shadow_patch_{}_{}", std::process::id(), nonce);
    rewrite_shadow_manifest(&root.join("Cargo.toml"), &lib_name)?;

    let shadow_source = root.join(relative_source);
    let wrapper = shadow_method_wrapper(patch_symbol, old_symbol, &impl_type, &function.signature)?;
    let mut file = fs::OpenOptions::new().append(true).open(&shadow_source)?;
    writeln!(file, "\n{wrapper}")?;

    println!(
        "hr: shadow patch crate {} source {} impl {}",
        root.display(),
        relative_source.display(),
        impl_type
    );

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let mut command = Command::new(cargo_command());
    command
        .arg("build")
        .arg("--lib")
        .current_dir(&root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", &target_dir)
        .env_remove("DYLD_INSERT_LIBRARIES");
    let status = command.status()?;
    if !status.success() {
        return Err(format!("shadow patch cargo build exited with {status}").into());
    }

    let dylib = target_dir.join("debug").join(dylib_filename(&lib_name));
    if !dylib.is_file() {
        return Err(format!("shadow patch dylib missing: {}", dylib.display()).into());
    }
    Ok(BuiltLivePatch {
        root,
        dylib,
        stubs: Vec::new(),
        cleanup_root: true,
    })
}

fn build_shadow_stub_patch_dylib(
    workspace_root: &Path,
    executable: &Path,
    source_uri: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
    prune_shadow: bool,
    fake_target_methods: bool,
    cached_symbol_resolver: Option<&BinarySymbolResolver>,
) -> Result<BuiltLivePatch, Box<dyn Error>> {
    let total_start = Instant::now();
    let source_path = file_uri_to_path(source_uri)?;
    let relative_source = source_path.strip_prefix(workspace_root)?;
    let start = Instant::now();
    let source_text = fs::read_to_string(&source_path)?;
    let impl_type =
        containing_impl_type(&source_text, function.signature_start).ok_or_else(|| {
            format!(
                "could not find containing impl type for method `{old_symbol}` in {}",
                source_path.display()
            )
        })?;
    log_timing("shadow-source-read-impl", start);
    let requested_stubs = shadow_stub_symbols(old_symbol);
    if requested_stubs.is_empty() {
        return Err(format!(
            "{SHADOW_STUBS_ENV} selected shadow-stub backend but no stubs were requested"
        )
        .into());
    }
    let owned_symbol_resolver;
    let symbol_resolver = match cached_symbol_resolver {
        Some(resolver) => {
            log_timing("shadow-symbol-index-reuse", Instant::now());
            resolver
        }
        None => {
            let start = Instant::now();
            owned_symbol_resolver = BinarySymbolResolver::load(workspace_root, executable)?;
            log_timing("shadow-symbol-index", start);
            &owned_symbol_resolver
        }
    };

    let persistent_fake = fake_target_methods && env_flag(SHADOW_PERSISTENT_ENV);
    let stable_fake = if persistent_fake {
        Some(persistent_shadow_crate(workspace_root, old_symbol)?)
    } else {
        None
    };
    let hot_update_only = stable_fake
        .as_ref()
        .map(|stable| fake_shadow_crate_ready(&stable.root))
        .unwrap_or(false);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging_root = std::env::temp_dir().join(format!(
        "hot-rust-shadow-stub-{}-{}-{nonce}",
        if persistent_fake { "stage" } else { "patch" },
        std::process::id()
    ));
    let staging_root = if hot_update_only {
        println!("hr: shadow-fake persistent skeleton reused");
        None
    } else {
        let start = Instant::now();
        if fake_target_methods {
            copy_workspace_for_fake_shadow(workspace_root, &staging_root)?;
        } else {
            copy_workspace_for_shadow(workspace_root, &staging_root)?;
        }
        let staging_root = staging_root.canonicalize()?;
        log_timing("shadow-stage-copy", start);
        Some(staging_root)
    };

    let start = Instant::now();
    let (build_root, lib_name, cleanup_root) = if let Some(stable) = &stable_fake {
        (stable.root.clone(), stable.lib_name.clone(), false)
    } else {
        let staging_root = staging_root
            .as_ref()
            .ok_or("non-persistent shadow build missing staging root")?;
        (
            staging_root.clone(),
            format!(
                "hot_rust_shadow_stub_patch_{}_{}",
                std::process::id(),
                nonce
            ),
            true,
        )
    };
    if let Some(staging_root) = &staging_root {
        rewrite_shadow_manifest(&staging_root.join("Cargo.toml"), &lib_name)?;
    }
    log_timing("shadow-manifest", start);

    let shadow_source = if hot_update_only {
        build_root.join(relative_source)
    } else {
        staging_root
            .as_ref()
            .ok_or("shadow build missing staging source root")?
            .join(relative_source)
    };
    let start = Instant::now();
    let mut shadow_text = if hot_update_only {
        source_text.clone()
    } else {
        fs::read_to_string(&shadow_source)?
    };
    let shadow_function = extract_function(&shadow_text, old_symbol).ok_or_else(|| {
        format!(
            "could not reparse shadow function `{old_symbol}` in {}",
            shadow_source.display()
        )
    })?;
    log_timing("shadow-target-parse", start);

    let start = Instant::now();
    let mut patched_body = shadow_function.body.clone();
    let mut stubs = Vec::new();
    let mut stub_sources = String::new();
    for source_symbol in requested_stubs {
        let needle = format!("{source_symbol}(");
        if !patched_body.contains(&needle) {
            println!(
                "hr: shadow-stub source {} did not call {}; skipping",
                old_symbol, source_symbol
            );
            continue;
        }
        let helper = extract_function(&shadow_text, &source_symbol).ok_or_else(|| {
            format!(
                "shadow-stub helper `{source_symbol}` not found in {}",
                shadow_source.display()
            )
        })?;
        let stub_symbol = format!("hot_rust_stub_{source_symbol}");
        let old_helper_symbol = symbol_resolver.symbol_for(&source_symbol)?;
        let stub_signature = patch_signature(&source_symbol, &stub_symbol, &helper.signature)?;
        patched_body = patched_body.replace(&needle, &format!("{stub_symbol}("));
        stub_sources.push_str(&format!(
            "\n#[no_mangle]\n#[inline(never)]\n{} {{\n    panic!(\"hot-rust shadow stub {source_symbol} called before runtime patch\")\n}}\n",
            stub_signature.trim()
        ));
        stubs.push(PatchStub {
            source_symbol,
            stub_symbol,
            old_symbol: old_helper_symbol,
        });
    }

    if stubs.is_empty() {
        return Err(format!(
            "shadow-stub backend found no requested helper calls in `{old_symbol}`"
        )
        .into());
    }
    log_timing("shadow-free-stubs", start);

    shadow_text.replace_range(
        shadow_function.body_start..shadow_function.body_end,
        &patched_body,
    );

    let mut skip_target_prune_names = vec![old_symbol.to_string()];
    if fake_target_methods {
        let start = Instant::now();
        let method_stubs = rewrite_target_method_callees_as_stubs(
            &symbol_resolver,
            &mut shadow_text,
            old_symbol,
            &impl_type,
            &patched_body,
        )?;
        skip_target_prune_names.extend(method_stubs.skip_names.iter().cloned());
        stub_sources.push_str(&method_stubs.stub_sources);
        stubs.extend(method_stubs.stubs);
        println!(
            "hr: shadow-fake exported {} method stubs from {}",
            method_stubs.stub_count,
            relative_source.display()
        );
        log_timing("shadow-method-stubs", start);
    }

    let start = Instant::now();
    let wrapper = shadow_method_wrapper(patch_symbol, old_symbol, &impl_type, &function.signature)?;
    shadow_text.push_str("\n");
    shadow_text.push_str(&wrapper);
    shadow_text.push_str(&stub_sources);
    log_timing("shadow-wrapper", start);

    if fake_target_methods {
        let start = Instant::now();
        skip_target_prune_names.push(patch_symbol.to_string());
        skip_target_prune_names.extend(stubs.iter().map(|stub| stub.stub_symbol.clone()));
        let (next, pruned) =
            prune_function_bodies_in_source_except(&shadow_text, &skip_target_prune_names);
        if pruned > 0 {
            println!(
                "hr: shadow-fake pruned {} non-live function bodies in {}",
                pruned,
                relative_source.display()
            );
        }
        shadow_text = next;
        log_timing("shadow-target-prune", start);
    }

    let start = Instant::now();
    if let Some(parent) = shadow_source.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&shadow_source, shadow_text)?;
    log_timing("shadow-target-write", start);

    if prune_shadow && !hot_update_only {
        let start = Instant::now();
        let staging_root = staging_root
            .as_ref()
            .ok_or("shadow tree prune requested without staging root")?;
        let report = prune_shadow_function_bodies(
            staging_root,
            &shadow_source,
            old_symbol,
            fake_target_methods,
        )?;
        println!(
            "hr: shadow-mini pruned {} function bodies across {} files; preserved prefixes={} mode={}",
            report.functions_pruned,
            report.files_touched,
            report.preserve_prefixes.join(","),
            if report.pruned_preserved { "all" } else { "outside-preserved" }
        );
        log_timing("shadow-tree-prune", start);
    } else if prune_shadow {
        log_timing("shadow-tree-prune-skip", Instant::now());
    }
    if fake_target_methods && !hot_update_only {
        let staging_root = staging_root
            .as_ref()
            .ok_or("shadow fake cleanup requested without staging root")?;
        let start = Instant::now();
        let report = strip_fake_serde_derives(staging_root)?;
        println!(
            "hr: shadow-fake stripped {} serde derive entries and {} serde attrs across {} files",
            report.derive_entries_removed, report.serde_attrs_removed, report.files_touched
        );
        log_timing("shadow-serde-strip", start);
        let start = Instant::now();
        write_fake_crate_root(staging_root)?;
        println!("hr: shadow-fake pruned crate root modules");
        log_timing("shadow-crate-root", start);
    } else if fake_target_methods {
        log_timing("shadow-serde-strip-skip", Instant::now());
        log_timing("shadow-crate-root-skip", Instant::now());
    }

    let root = if persistent_fake {
        if hot_update_only {
            println!(
                "hr: shadow-fake persistent live source updated {}",
                relative_source.display()
            );
            log_timing("shadow-persistent-sync-skip", Instant::now());
        } else {
            let staging_root = staging_root
                .as_ref()
                .ok_or("persistent shadow sync requested without staging root")?;
            let start = Instant::now();
            let sync = sync_shadow_tree_changed(staging_root, &build_root)?;
            println!(
                "hr: shadow-fake persistent crate {} synced files={} bytes={} lib={}",
                build_root.display(),
                sync.files_copied,
                sync.bytes_copied,
                lib_name
            );
            log_timing("shadow-persistent-sync", start);
            let start = Instant::now();
            let _ = fs::remove_dir_all(staging_root);
            log_timing("shadow-stage-cleanup", start);
        }
        build_root.canonicalize()?
    } else {
        staging_root.ok_or("non-persistent shadow build missing root")?
    };

    println!(
        "hr: shadow-stub patch crate {} source {} impl {} stubs={}",
        root.display(),
        relative_source.display(),
        impl_type,
        stubs
            .iter()
            .map(|stub| format!("{}->{}", stub.stub_symbol, stub.old_symbol))
            .collect::<Vec<_>>()
            .join(",")
    );

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let mut command = Command::new(cargo_command());
    command
        .arg("build")
        .arg("--lib")
        .current_dir(&root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env("CARGO_TARGET_DIR", &target_dir)
        .env_remove("DYLD_INSERT_LIBRARIES");
    if !persistent_fake {
        command.env("CARGO_INCREMENTAL", "0");
    }
    let start = Instant::now();
    let status = command.status()?;
    let elapsed = start.elapsed();
    if !status.success() {
        return Err(format!("shadow-stub patch cargo build exited with {status}").into());
    }
    log_timing("shadow-cargo-build", start);

    let built_dylib = target_dir.join("debug").join(dylib_filename(&lib_name));
    let start = Instant::now();
    let dylib = if persistent_fake {
        copy_unique_patch_dylib(&built_dylib, &lib_name, nonce)?
    } else {
        built_dylib
    };
    log_timing("shadow-dylib-copy", start);
    if !dylib.is_file() {
        return Err(format!("shadow-stub patch dylib missing: {}", dylib.display()).into());
    }
    println!(
        "hr: shadow-stub build elapsed={:.2}s",
        elapsed.as_secs_f64()
    );
    log_timing("shadow-total", total_start);
    Ok(BuiltLivePatch {
        root,
        dylib,
        stubs,
        cleanup_root,
    })
}

struct ShadowPruneReport {
    files_touched: usize,
    functions_pruned: usize,
    preserve_prefixes: Vec<String>,
    pruned_preserved: bool,
}

struct ShadowSerdeStripReport {
    files_touched: usize,
    derive_entries_removed: usize,
    serde_attrs_removed: usize,
}

struct PersistentShadowCrate {
    root: PathBuf,
    lib_name: String,
}

struct ShadowSyncReport {
    files_copied: usize,
    bytes_copied: u64,
}

struct ShadowMethodStubRewrite {
    stubs: Vec<PatchStub>,
    stub_sources: String,
    skip_names: Vec<String>,
    stub_count: usize,
}

struct MethodStubPlan {
    body_start: usize,
    body_end: usize,
    body_replacement: String,
    stub_source: String,
    patch_stub: PatchStub,
    skip_names: Vec<String>,
}

fn persistent_shadow_crate(
    workspace_root: &Path,
    old_symbol: &str,
) -> Result<PersistentShadowCrate, Box<dyn Error>> {
    let canonical = workspace_root.canonicalize()?;
    let hash = stable_hash(canonical.to_string_lossy().as_ref());
    let symbol = rust_ident_fragment(old_symbol);
    let lib_name = format!("hot_rust_shadow_fake_{symbol}_{hash:016x}");
    let target_dir = configured_target_dir(workspace_root);
    let root = target_dir
        .join("hot-rust-shadow-fake")
        .join(format!("{symbol}-{hash:016x}"));
    Ok(PersistentShadowCrate { root, lib_name })
}

fn fake_shadow_crate_ready(root: &Path) -> bool {
    root.join("Cargo.toml").is_file() && root.join("src/lib.rs").is_file()
}

fn configured_target_dir(workspace_root: &Path) -> PathBuf {
    if let Some(raw) = std::env::var_os("CARGO_TARGET_DIR") {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return path;
        }
        return workspace_root.join(path);
    }
    workspace_root.join("target")
}

fn stable_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn rust_ident_fragment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out
        .chars()
        .next()
        .map(|ch| ch.is_ascii_digit())
        .unwrap_or(true)
    {
        out.insert(0, '_');
    }
    out
}

fn sync_shadow_tree_changed(src: &Path, dst: &Path) -> io::Result<ShadowSyncReport> {
    let mut report = ShadowSyncReport {
        files_copied: 0,
        bytes_copied: 0,
    };
    sync_shadow_tree_changed_inner(src, dst, &mut report)?;
    Ok(report)
}

fn sync_shadow_tree_changed_inner(
    src: &Path,
    dst: &Path,
    report: &mut ShadowSyncReport,
) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            sync_shadow_tree_changed_inner(&path, &target, report)?;
        } else if file_type.is_file() {
            let bytes = fs::read(&path)?;
            let changed = fs::read(&target)
                .map(|existing| existing != bytes)
                .unwrap_or(true);
            if changed {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &bytes)?;
                report.files_copied += 1;
                report.bytes_copied += bytes.len() as u64;
            }
        }
    }
    Ok(())
}

fn copy_unique_patch_dylib(
    dylib: &Path,
    lib_name: &str,
    nonce: u128,
) -> Result<PathBuf, Box<dyn Error>> {
    if !dylib.is_file() {
        return Err(format!("shadow-stub patch dylib missing: {}", dylib.display()).into());
    }
    let dir = std::env::temp_dir().join("hot-rust-patch-dylibs");
    fs::create_dir_all(&dir)?;
    let unique = dir.join(dylib_filename(&format!("{lib_name}_{nonce}")));
    fs::copy(dylib, &unique)?;
    Ok(unique)
}

fn write_fake_crate_root(root: &Path) -> io::Result<()> {
    fs::write(
        root.join("src/lib.rs"),
        r##"//! hot-rust generated fake crate root.

pub mod error;
pub mod model;
pub mod ole_chart;
pub mod ooxml_chart;
pub mod paint;
pub mod renderer;

pub mod parser {
    #[derive(Debug)]
    pub struct ParseError;

    impl std::fmt::Display for ParseError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("hot-rust fake ParseError")
        }
    }

    pub mod hwpx {
        #[derive(Debug)]
        pub struct HwpxError;

        impl std::fmt::Display for HwpxError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("hot-rust fake HwpxError")
            }
        }
    }
}

pub mod serializer {
    #[derive(Debug)]
    pub struct SerializeError;

    impl std::fmt::Display for SerializeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("hot-rust fake SerializeError")
        }
    }
}

pub mod document_core {
    pub struct DocumentCore;

    pub mod helpers {
        pub(crate) fn json_escape(s: &str) -> String {
            let mut out = String::with_capacity(s.len());
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
                    ch => out.push(ch),
                }
            }
            out
        }

        pub(crate) fn color_ref_to_css(color: crate::model::ColorRef) -> String {
            let r = color & 0xFF;
            let g = (color >> 8) & 0xFF;
            let b = (color >> 16) & 0xFF;
            format!("#{r:02x}{g:02x}{b:02x}")
        }
    }
}
"##,
    )
}

fn rewrite_target_method_callees_as_stubs(
    symbol_resolver: &BinarySymbolResolver,
    shadow_text: &mut String,
    old_symbol: &str,
    impl_type: &str,
    patched_body: &str,
) -> Result<ShadowMethodStubRewrite, Box<dyn Error>> {
    let mut plans = Vec::new();
    for callee in direct_same_impl_callees(patched_body) {
        if callee == old_symbol {
            continue;
        }
        let Some(function) = extract_function(shadow_text, &callee) else {
            continue;
        };
        if containing_impl_type(shadow_text, function.signature_start).as_deref() != Some(impl_type)
        {
            continue;
        }
        let stub_symbol = format!("hot_rust_stub_method_{callee}");
        let old_method_symbol = symbol_resolver.symbol_for(&callee)?;
        let plan = method_stub_plan(
            impl_type,
            &callee,
            &stub_symbol,
            &old_method_symbol,
            &function,
            shadow_text,
        )?;
        plans.push(plan);
    }

    plans.sort_by_key(|plan| plan.body_start);
    plans.dedup_by_key(|plan| plan.patch_stub.source_symbol.clone());

    let mut stubs = Vec::new();
    let mut stub_sources = String::new();
    let mut skip_names = Vec::new();
    for plan in plans.iter().rev() {
        shadow_text.replace_range(plan.body_start..plan.body_end, &plan.body_replacement);
    }
    for plan in plans {
        stub_sources.push_str(&plan.stub_source);
        stubs.push(plan.patch_stub);
        skip_names.extend(plan.skip_names);
    }
    skip_names.sort();
    skip_names.dedup();
    let stub_count = stubs.len();

    Ok(ShadowMethodStubRewrite {
        stubs,
        stub_sources,
        skip_names,
        stub_count,
    })
}

fn method_stub_plan(
    impl_type: &str,
    method_name: &str,
    stub_symbol: &str,
    old_method_symbol: &str,
    function: &ParsedFunction,
    source: &str,
) -> Result<MethodStubPlan, Box<dyn Error>> {
    let open = function
        .signature
        .find('(')
        .ok_or_else(|| format!("method `{method_name}` signature has no `(`"))?;
    let close = matching_paren(&function.signature, open)
        .ok_or_else(|| format!("method `{method_name}` signature has no matching `)`"))?;
    let params = split_top_level_commas(&function.signature[open + 1..close]);
    let has_receiver = method_receiver(&function.signature).is_some();
    let stub_signature = if has_receiver {
        method_object_probe_signature(stub_symbol, impl_type, &function.signature)?
    } else {
        patch_signature(method_name, stub_symbol, &function.signature)?
    };

    let mut call_args = Vec::new();
    let value_params = if has_receiver {
        call_args.push("self".to_string());
        &params[1..]
    } else {
        &params[..]
    };
    for param in value_params {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        call_args.push(param_binding_name(param)?);
    }

    let indent = body_indent_for(source, function.body_start);
    let body_replacement = format!("\n{indent}{}({})\n", stub_symbol, call_args.join(", "));
    let stub_source = format!(
        "\n#[no_mangle]\n#[inline(never)]\n{} {{\n    panic!(\"hot-rust shadow fake method stub {method_name} called before runtime patch\")\n}}\n",
        stub_signature.trim()
    );
    Ok(MethodStubPlan {
        body_start: function.body_start,
        body_end: function.body_end,
        body_replacement,
        stub_source,
        patch_stub: PatchStub {
            source_symbol: method_name.to_string(),
            stub_symbol: stub_symbol.to_string(),
            old_symbol: old_method_symbol.to_string(),
        },
        skip_names: vec![method_name.to_string(), stub_symbol.to_string()],
    })
}

fn direct_same_impl_callees(body: &str) -> Vec<String> {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        LineComment,
        BlockComment(usize),
        String { escaped: bool },
        Char { escaped: bool },
        RawString { hashes: usize },
    }

    let bytes = body.as_bytes();
    let mut names = Vec::new();
    let mut state = State::Normal;
    let mut index = 0usize;
    while index < bytes.len() {
        match state {
            State::Normal => {
                if bytes[index..].starts_with(b"//") {
                    state = State::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((hashes, len)) = raw_string_start(&bytes[index..]) {
                    state = State::RawString { hashes };
                    index += len;
                    continue;
                }
                if bytes[index] == b'"' {
                    state = State::String { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index] == b'\'' && looks_like_char_start(bytes, index) {
                    state = State::Char { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index..].starts_with(b"self.") {
                    if let Some((name, next)) = method_call_name_after(bytes, index + 5) {
                        names.push(name);
                        index = next;
                        continue;
                    }
                }
                if bytes[index..].starts_with(b"Self::") {
                    if let Some((name, next)) = method_call_name_after(bytes, index + 6) {
                        names.push(name);
                        index = next;
                        continue;
                    }
                }
                index += 1;
            }
            State::LineComment => {
                if bytes[index] == b'\n' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(depth + 1);
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    if depth == 1 {
                        state = State::Normal;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            State::String { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::String { escaped: false };
                } else if byte == b'\\' {
                    state = State::String { escaped: true };
                } else if byte == b'"' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::Char { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::Char { escaped: false };
                } else if byte == b'\\' {
                    state = State::Char { escaped: true };
                } else if byte == b'\'' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::RawString { hashes } => {
                if bytes[index] == b'"'
                    && index + 1 + hashes <= bytes.len()
                    && bytes[index + 1..index + 1 + hashes]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    state = State::Normal;
                    index += 1 + hashes;
                } else {
                    index += 1;
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn method_call_name_after(bytes: &[u8], mut index: usize) -> Option<(String, usize)> {
    let start = index;
    let first = *bytes.get(index)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .copied()
        .map(is_ident_byte)
        .unwrap_or(false)
    {
        index += 1;
    }
    let name = std::str::from_utf8(&bytes[start..index]).ok()?.to_string();
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    (bytes.get(index) == Some(&b'(')).then_some((name, index))
}

fn prune_shadow_function_bodies(
    root: &Path,
    target_source: &Path,
    old_symbol: &str,
    prune_preserved: bool,
) -> Result<ShadowPruneReport, Box<dyn Error>> {
    let preserve_prefixes = shadow_preserve_prefixes(old_symbol);
    let src = root.join("src");
    let mut stack = vec![src];
    let mut files_touched = 0usize;
    let mut functions_pruned = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            if path == target_source {
                continue;
            }
            let relative = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if !prune_preserved
                && preserve_prefixes.iter().any(|prefix| {
                    relative == *prefix || relative.starts_with(&format!("{prefix}/"))
                })
            {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let (next, count) = prune_function_bodies_in_source(&text);
            if count == 0 {
                continue;
            }
            fs::write(&path, next)?;
            files_touched += 1;
            functions_pruned += count;
        }
    }
    Ok(ShadowPruneReport {
        files_touched,
        functions_pruned,
        preserve_prefixes,
        pruned_preserved: prune_preserved,
    })
}

fn strip_fake_serde_derives(root: &Path) -> Result<ShadowSerdeStripReport, Box<dyn Error>> {
    let src = root.join("src");
    let mut stack = vec![src];
    let mut files_touched = 0usize;
    let mut derive_entries_removed = 0usize;
    let mut serde_attrs_removed = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let (next, removed_derives, removed_attrs) = strip_serde_from_source(&text);
            if removed_derives == 0 && removed_attrs == 0 {
                continue;
            }
            fs::write(&path, next)?;
            files_touched += 1;
            derive_entries_removed += removed_derives;
            serde_attrs_removed += removed_attrs;
        }
    }
    Ok(ShadowSerdeStripReport {
        files_touched,
        derive_entries_removed,
        serde_attrs_removed,
    })
}

fn strip_serde_from_source(source: &str) -> (String, usize, usize) {
    let mut out = String::with_capacity(source.len());
    let mut derive_entries_removed = 0usize;
    let mut serde_attrs_removed = 0usize;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[serde") {
            serde_attrs_removed += 1;
            continue;
        }
        if let Some((next_line, removed)) = strip_serde_from_derive_line(line) {
            derive_entries_removed += removed;
            if !next_line.is_empty() {
                out.push_str(&next_line);
                out.push('\n');
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !source.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    (out, derive_entries_removed, serde_attrs_removed)
}

fn strip_serde_from_derive_line(line: &str) -> Option<(String, usize)> {
    let derive_start = line.find("#[derive(")?;
    let args_start = derive_start + "#[derive(".len();
    let args_end = line[args_start..].find(")]")? + args_start;
    let args = &line[args_start..args_end];
    let mut removed = 0usize;
    let kept = split_top_level_commas(args)
        .into_iter()
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            if is_serde_derive_entry(entry) {
                removed += 1;
                None
            } else {
                Some(entry.to_string())
            }
        })
        .collect::<Vec<_>>();
    if removed == 0 {
        return None;
    }
    if kept.is_empty() {
        return Some((String::new(), removed));
    }
    let mut next = String::new();
    next.push_str(&line[..args_start]);
    next.push_str(&kept.join(", "));
    next.push_str(&line[args_end..]);
    Some((next, removed))
}

fn is_serde_derive_entry(entry: &str) -> bool {
    matches!(
        entry,
        "Serialize" | "Deserialize" | "serde::Serialize" | "serde::Deserialize"
    )
}

fn shadow_preserve_prefixes(old_symbol: &str) -> Vec<String> {
    let raw = std::env::var(SHADOW_PRESERVE_ENV).unwrap_or_else(|_| {
        if old_symbol == "render_node" {
            "src/renderer,src/model,src/paint,src/ole_chart,src/ooxml_chart".to_string()
        } else {
            String::new()
        }
    });
    raw.split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.trim_end_matches('/').to_string())
        .collect()
}

fn prune_function_bodies_in_source(source: &str) -> (String, usize) {
    prune_function_bodies_in_source_except(source, &[])
}

fn prune_function_bodies_in_source_except(source: &str, skip_names: &[String]) -> (String, usize) {
    let mut ranges = Vec::new();
    let mut search = 0usize;
    for fn_start in function_item_positions(source) {
        if fn_start < search {
            continue;
        }
        if !looks_like_function_item(source, fn_start) {
            continue;
        }
        let Some(brace) = source[fn_start..].find('{').map(|pos| fn_start + pos) else {
            continue;
        };
        let header = &source[fn_start..brace];
        if header.contains(';') || header.contains(" = ") {
            continue;
        }
        if function_prefix_line(source, fn_start).contains("const") {
            continue;
        }
        if function_name_at(source, fn_start)
            .as_ref()
            .map(|name| skip_names.iter().any(|skip| skip == name))
            .unwrap_or(false)
        {
            continue;
        }
        let Some(end) = matching_code_brace(source, brace) else {
            continue;
        };
        ranges.push((brace + 1, end));
        search = end + 1;
    }

    if ranges.is_empty() {
        return (source.to_string(), 0);
    }

    let mut out = source.to_string();
    for (start, end) in ranges.iter().rev() {
        let indent = body_indent_for(&out, *start);
        out.replace_range(
            *start..*end,
            &format!("\n{indent}unimplemented!(\"hot-rust pruned shadow body\")\n"),
        );
    }
    let count = ranges.len();
    (out, count)
}

fn function_name_at(source: &str, fn_start: usize) -> Option<String> {
    let bytes = source.as_bytes();
    if !bytes.get(fn_start..)?.starts_with(b"fn") {
        return None;
    }
    let mut index = fn_start + 2;
    while bytes
        .get(index)
        .copied()
        .map(|byte| byte.is_ascii_whitespace())
        .unwrap_or(false)
    {
        index += 1;
    }
    let start = index;
    let first = *bytes.get(index)?;
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return None;
    }
    index += 1;
    while bytes
        .get(index)
        .copied()
        .map(is_ident_byte)
        .unwrap_or(false)
    {
        index += 1;
    }
    std::str::from_utf8(&bytes[start..index])
        .ok()
        .map(ToString::to_string)
}

fn matching_code_brace(source: &str, open: usize) -> Option<usize> {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        LineComment,
        BlockComment(usize),
        String { escaped: bool },
        Char { escaped: bool },
        RawString { hashes: usize },
    }

    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut state = State::Normal;
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match state {
            State::Normal => {
                if bytes[index..].starts_with(b"//") {
                    state = State::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((hashes, len)) = raw_string_start(&bytes[index..]) {
                    state = State::RawString { hashes };
                    index += len;
                    continue;
                }
                match bytes[index] {
                    b'"' => {
                        state = State::String { escaped: false };
                        index += 1;
                    }
                    b'\'' if looks_like_char_start(bytes, index) => {
                        state = State::Char { escaped: false };
                        index += 1;
                    }
                    b'{' => {
                        depth += 1;
                        index += 1;
                    }
                    b'}' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(index);
                        }
                        index += 1;
                    }
                    _ => index += 1,
                }
            }
            State::LineComment => {
                if bytes[index] == b'\n' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(depth + 1);
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    if depth == 1 {
                        state = State::Normal;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            State::String { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::String { escaped: false };
                } else if byte == b'\\' {
                    state = State::String { escaped: true };
                } else if byte == b'"' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::Char { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::Char { escaped: false };
                } else if byte == b'\\' {
                    state = State::Char { escaped: true };
                } else if byte == b'\'' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::RawString { hashes } => {
                if bytes[index] == b'"'
                    && index + 1 + hashes <= bytes.len()
                    && bytes[index + 1..index + 1 + hashes]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    state = State::Normal;
                    index += 1 + hashes;
                } else {
                    index += 1;
                }
            }
        }
    }
    None
}

fn function_item_positions(source: &str) -> Vec<usize> {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        LineComment,
        BlockComment(usize),
        String { escaped: bool },
        Char { escaped: bool },
        RawString { hashes: usize },
    }

    let bytes = source.as_bytes();
    let mut positions = Vec::new();
    let mut state = State::Normal;
    let mut index = 0usize;
    while index < bytes.len() {
        match state {
            State::Normal => {
                if bytes[index..].starts_with(b"//") {
                    state = State::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((hashes, len)) = raw_string_start(&bytes[index..]) {
                    state = State::RawString { hashes };
                    index += len;
                    continue;
                }
                if bytes[index] == b'"' {
                    state = State::String { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index] == b'\'' && looks_like_char_start(bytes, index) {
                    state = State::Char { escaped: false };
                    index += 1;
                    continue;
                }
                if bytes[index..].starts_with(b"fn ")
                    && index
                        .checked_sub(1)
                        .map(|pos| !is_ident_byte(bytes[pos]))
                        .unwrap_or(true)
                    && bytes
                        .get(index + 2)
                        .map(|byte| byte.is_ascii_whitespace())
                        .unwrap_or(false)
                {
                    positions.push(index);
                    index += 3;
                    continue;
                }
                index += 1;
            }
            State::LineComment => {
                if bytes[index] == b'\n' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = State::BlockComment(depth + 1);
                    index += 2;
                } else if bytes[index..].starts_with(b"*/") {
                    if depth == 1 {
                        state = State::Normal;
                    } else {
                        state = State::BlockComment(depth - 1);
                    }
                    index += 2;
                } else {
                    index += 1;
                }
            }
            State::String { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::String { escaped: false };
                } else if byte == b'\\' {
                    state = State::String { escaped: true };
                } else if byte == b'"' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::Char { escaped } => {
                let byte = bytes[index];
                if escaped {
                    state = State::Char { escaped: false };
                } else if byte == b'\\' {
                    state = State::Char { escaped: true };
                } else if byte == b'\'' {
                    state = State::Normal;
                }
                index += 1;
            }
            State::RawString { hashes } => {
                if bytes[index] == b'"'
                    && index + 1 + hashes <= bytes.len()
                    && bytes[index + 1..index + 1 + hashes]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    state = State::Normal;
                    index += 1 + hashes;
                } else {
                    index += 1;
                }
            }
        }
    }
    positions
}

fn raw_string_start(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    if bytes.get(offset) == Some(&b'b') {
        offset += 1;
    }
    if bytes.get(offset) != Some(&b'r') {
        return None;
    }
    offset += 1;
    let mut hashes = 0usize;
    while bytes.get(offset + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if bytes.get(offset + hashes) == Some(&b'"') {
        Some((hashes, offset + hashes + 1))
    } else {
        None
    }
}

fn looks_like_char_start(bytes: &[u8], index: usize) -> bool {
    if index + 2 >= bytes.len() {
        return false;
    }
    if let Some(next) = bytes.get(index + 1).copied() {
        if (next == b'_' || next.is_ascii_alphabetic()) && bytes.get(index + 2) != Some(&b'\'') {
            return false;
        }
    }
    let prev = index
        .checked_sub(1)
        .and_then(|pos| bytes.get(pos).copied())
        .unwrap_or(b' ');
    if is_ident_byte(prev) {
        return false;
    }
    true
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn looks_like_function_item(source: &str, fn_start: usize) -> bool {
    let prefix = function_prefix_line(source, fn_start);
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return true;
    }
    let allowed = [
        "pub",
        "pub(crate)",
        "pub(super)",
        "async",
        "const",
        "unsafe",
        "extern",
        "default",
    ];
    trimmed
        .split_whitespace()
        .all(|part| allowed.contains(&part) || part.starts_with("pub(") || part.starts_with('"'))
}

fn function_prefix_line(source: &str, fn_start: usize) -> &str {
    let line_start = source[..fn_start]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    &source[line_start..fn_start]
}

fn body_indent_for(source: &str, body_start: usize) -> String {
    let line_start = source[..body_start]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let prefix = source[line_start..body_start]
        .chars()
        .take_while(|ch| ch.is_whitespace() && *ch != '\n')
        .collect::<String>();
    format!("{prefix}    ")
}

fn shadow_stub_symbols(old_symbol: &str) -> Vec<String> {
    let raw = std::env::var(SHADOW_STUBS_ENV).unwrap_or_else(|_| {
        if old_symbol == "render_node" {
            "escape_xml,color_to_svg".to_string()
        } else {
            String::new()
        }
    });
    raw.split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn method_receiver(signature: &str) -> Option<String> {
    let open = signature.find('(')?;
    let mut depth = 0usize;
    let mut end = None;
    for (offset, ch) in signature[open + 1..].char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => {
                if depth == 0 {
                    if ch == ')' {
                        end = Some(open + 1 + offset);
                        break;
                    }
                } else {
                    depth -= 1;
                }
            }
            ',' if depth == 0 => {
                end = Some(open + 1 + offset);
                break;
            }
            _ => {}
        }
    }
    let first = signature[open + 1..end?].trim();
    let normalized = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let is_receiver = matches!(normalized.as_str(), "self" | "mut self")
        || normalized.starts_with("&self")
        || normalized.starts_with("&mut self")
        || normalized.starts_with("self:")
        || normalized.starts_with("mut self:");
    is_receiver.then_some(normalized)
}

fn containing_impl_type(source: &str, signature_start: usize) -> Option<String> {
    let mut best = None;
    for (impl_start, _) in source[..signature_start].match_indices("impl ") {
        let brace = source[impl_start..].find('{')? + impl_start;
        if brace >= signature_start {
            continue;
        }
        let close = matching_brace(source, brace)?;
        if close > signature_start {
            let header = source[impl_start..brace].trim();
            if let Some(impl_type) = impl_type_from_header(header) {
                best = Some((impl_start, impl_type));
            }
        }
    }
    best.map(|(_, impl_type)| impl_type)
}

fn impl_type_from_header(header: &str) -> Option<String> {
    let mut rest = header.strip_prefix("impl")?.trim();
    if rest.starts_with('<') {
        let generic_end = matching_angle(rest, 0)?;
        rest = rest[generic_end + 1..].trim();
    }
    if let Some(for_index) = rest.rfind(" for ") {
        rest = rest[for_index + " for ".len()..].trim();
    }
    rest = rest.split(" where ").next().unwrap_or(rest).trim();
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn matching_angle(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn shadow_method_wrapper(
    patch_symbol: &str,
    old_symbol: &str,
    impl_type: &str,
    signature: &str,
) -> Result<String, Box<dyn Error>> {
    let open = signature.find('(').ok_or("method signature has no `(`")?;
    let close = matching_paren(signature, open).ok_or("method signature has no matching `)`")?;
    let params = split_top_level_commas(&signature[open + 1..close]);
    let Some(receiver) = params.first().map(|param| param.trim()) else {
        return Err(format!("method `{old_symbol}` has no receiver").into());
    };
    let this_param = match receiver
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .as_str()
    {
        "&mut self" => format!("this: &mut {impl_type}"),
        "&self" => format!("this: &{impl_type}"),
        "self" => format!("this: {impl_type}"),
        "mut self" => format!("mut this: {impl_type}"),
        other => return Err(format!("unsupported method receiver `{other}`").into()),
    };

    let mut wrapper_params = vec![this_param];
    let mut call_args = Vec::new();
    for param in params.iter().skip(1) {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        wrapper_params.push(param.to_string());
        call_args.push(param_binding_name(param)?);
    }

    let tail = signature[close + 1..].trim();
    let tail = if tail.is_empty() {
        String::new()
    } else {
        format!(" {tail}")
    };
    Ok(format!(
        "#[no_mangle]\n#[inline(never)]\npub fn {patch_symbol}({}){tail} {{\n    this.{old_symbol}({})\n}}\n",
        wrapper_params.join(", "),
        call_args.join(", ")
    ))
}

fn matching_paren(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(source: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut angle = 0usize;
    for (index, ch) in source.char_indices() {
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket += 1,
            ']' => bracket = bracket.saturating_sub(1),
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            ',' if paren == 0 && bracket == 0 && angle == 0 => {
                parts.push(source[start..index].to_string());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(source[start..].to_string());
    parts
}

fn param_binding_name(param: &str) -> Result<String, Box<dyn Error>> {
    let Some((pattern, _ty)) = param.split_once(':') else {
        return Err(format!("parameter has no type annotation: `{param}`").into());
    };
    let name = pattern
        .split(|ch: char| !(ch == '_' || ch.is_ascii_alphanumeric()))
        .filter(|part| !part.is_empty())
        .filter(|part| !matches!(*part, "mut" | "ref"))
        .next_back()
        .ok_or_else(|| format!("could not extract parameter binding from `{param}`"))?;
    Ok(name.to_string())
}

fn copy_workspace_for_shadow(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();
        if matches!(file_name_str.as_ref(), ".git" | "target") {
            continue;
        }
        let target = dst.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_workspace_for_shadow(&path, &target)?;
        } else if file_type.is_file() {
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

fn copy_workspace_for_fake_shadow(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for file in [
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "rust-toolchain",
        "rust-toolchain.toml",
    ] {
        copy_relative_file_if_exists(src, dst, file)?;
    }
    for dir in ["src", "examples", ".cargo"] {
        let from = src.join(dir);
        if from.is_dir() {
            copy_workspace_for_shadow(&from, &dst.join(dir))?;
        }
    }

    // Some currently pruned bodies still leave compile-time include paths in
    // parser/serializer modules. Keep these tiny fixtures, not the whole repo.
    copy_relative_file_if_exists(src, dst, "saved/blank2010.hwp")?;
    let ref_dir = src.join("samples/hwpx/ref");
    if ref_dir.is_dir() {
        copy_workspace_for_shadow(&ref_dir, &dst.join("samples/hwpx/ref"))?;
    }
    Ok(())
}

fn copy_relative_file_if_exists(src: &Path, dst: &Path, relative: &str) -> io::Result<()> {
    let from = src.join(relative);
    if !from.is_file() {
        return Ok(());
    }
    let to = dst.join(relative);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(from, to)?;
    Ok(())
}

fn rewrite_shadow_manifest(manifest: &Path, lib_name: &str) -> io::Result<()> {
    let text = fs::read_to_string(manifest)?;
    let text = rewrite_package_name(&text, lib_name);
    let text = rewrite_lib_name(&text, lib_name);
    fs::write(manifest, text)
}

fn rewrite_package_name(text: &str, package_name: &str) -> String {
    rewrite_section_name_key(text, "package", package_name)
}

fn rewrite_lib_name(text: &str, lib_name: &str) -> String {
    if text.lines().any(|line| line.trim() == "[lib]") {
        rewrite_section_name_key(text, "lib", lib_name)
    } else {
        format!("{text}\n[lib]\nname = \"{lib_name}\"\ncrate-type = [\"cdylib\"]\n")
    }
}

fn rewrite_section_name_key(text: &str, section: &str, name: &str) -> String {
    let mut out = String::new();
    let mut in_section = false;
    let mut wrote_name = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_section && !wrote_name {
                out.push_str(&format!("name = \"{name}\"\n"));
            }
            in_section = trimmed == format!("[{section}]");
            wrote_name = false;
        }
        if in_section && trimmed.starts_with("name") && trimmed.contains('=') {
            out.push_str(&format!("name = \"{name}\"\n"));
            wrote_name = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if in_section && !wrote_name {
        out.push_str(&format!("name = \"{name}\"\n"));
    }
    out
}

fn dylib_filename(stem: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        format!("lib{stem}.dylib")
    }
    #[cfg(target_os = "linux")]
    {
        format!("lib{stem}.so")
    }
    #[cfg(target_os = "windows")]
    {
        format!("{stem}.dll")
    }
}

#[derive(Debug, Clone)]
struct ParsedFunction {
    signature: String,
    body: String,
    signature_start: usize,
    body_start: usize,
    body_end: usize,
}

fn extract_function(source: &str, symbol: &str) -> Option<ParsedFunction> {
    for fn_start in function_item_positions(source) {
        if !looks_like_function_item(source, fn_start) {
            continue;
        }
        if function_name_at(source, fn_start).as_deref() != Some(symbol) {
            continue;
        }
        return extract_function_at(source, fn_start);
    }
    None
}

fn extract_function_at(source: &str, fn_start: usize) -> Option<ParsedFunction> {
    let line_start = source[..fn_start]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let line_prefix = &source[line_start..fn_start];
    let signature_start = if line_prefix.trim().is_empty() {
        fn_start
    } else {
        line_start + line_prefix.len() - line_prefix.trim_start().len()
    };
    let brace = source[fn_start..].find('{')? + fn_start;
    let end = matching_code_brace(source, brace)?;
    Some(ParsedFunction {
        signature: source[signature_start..brace].trim().to_string(),
        body: source[brace + 1..end].to_string(),
        signature_start,
        body_start: brace + 1,
        body_end: end,
    })
}

fn patch_signature(
    old_symbol: &str,
    patch_symbol: &str,
    signature: &str,
) -> Result<String, Box<dyn Error>> {
    let needle = format!("fn {old_symbol}");
    let replacement = format!("fn {patch_symbol}");
    let renamed = signature.replacen(&needle, &replacement, 1);
    if renamed == signature {
        return Err(format!("signature does not contain `{needle}`: {signature}").into());
    }
    let trimmed = renamed.trim_start();
    if trimmed.starts_with("pub ") || trimmed.starts_with("pub(") {
        Ok(renamed)
    } else {
        Ok(format!("pub {renamed}"))
    }
}

fn split_run_args(args: &[String]) -> (&[String], &[String]) {
    if let Some(index) = args.iter().position(|arg| arg == "--") {
        (&args[..index], &args[index + 1..])
    } else {
        (args, &[])
    }
}

fn selected_bin_name(cargo_side: &[String]) -> Option<String> {
    let mut args = cargo_side.iter();
    while let Some(arg) = args.next() {
        if arg == "--bin" {
            return args.next().cloned();
        }
        if let Some(name) = arg.strip_prefix("--bin=") {
            return Some(name.to_string());
        }
    }
    None
}

fn cargo_bin_source_uri(
    workspace_root: &Path,
    bin_name: Option<&str>,
) -> Result<Option<String>, Box<dyn Error>> {
    let mut command = Command::new(cargo_command());
    command
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("RUSTFLAGS", merged_rustflags())
        .env_remove("DYLD_INSERT_LIBRARIES")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!("cargo metadata exited with {}", output.status).into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("cargo metadata missing packages")?;

    let mut first_bin = None;
    for package in packages {
        let Some(targets) = package.get("targets").and_then(Value::as_array) else {
            continue;
        };
        for target in targets {
            let is_bin = target
                .get("kind")
                .and_then(Value::as_array)
                .map(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")))
                .unwrap_or(false);
            if !is_bin {
                continue;
            }
            let name = target.get("name").and_then(Value::as_str).unwrap_or("");
            let Some(src_path) = target.get("src_path").and_then(Value::as_str) else {
                continue;
            };
            if bin_name == Some(name) {
                return Ok(Some(path_to_file_uri(Path::new(src_path))));
            }
            first_bin.get_or_insert_with(|| path_to_file_uri(Path::new(src_path)));
        }
    }

    Ok(if bin_name.is_none() { first_bin } else { None })
}

fn discover_live_source_uri(
    workspace_root: &Path,
    ra: &RustAnalyzerSession,
    symbol: &str,
    bin_name: Option<&str>,
    module_hint: Option<&[String]>,
) -> Result<String, Box<dyn Error>> {
    if let Some(uri) = ra.workspace_symbol_uri(symbol)? {
        match source_text_from_ra_or_disk(ra, &uri) {
            Ok(text)
                if extract_function(&text, symbol).is_some()
                    && module_hint
                        .map(|hint| uri_matches_module_hint(workspace_root, &uri, hint))
                        .unwrap_or(true) =>
            {
                return Ok(uri)
            }
            Ok(text) if extract_function(&text, symbol).is_some() => {
                println!("hr: ignoring rust-analyzer symbol URI outside runtime module hint: {uri}")
            }
            Ok(_) => println!("hr: ignoring rust-analyzer symbol URI without `fn {symbol}`: {uri}"),
            Err(err) => println!("hr: ignoring unreadable rust-analyzer symbol URI {uri}: {err}"),
        }
    }

    let candidates = scan_workspace_function_uris(workspace_root, symbol)?;
    if let Some(hint) = module_hint {
        let matches = candidates
            .iter()
            .filter(|uri| uri_matches_module_hint(workspace_root, uri, hint))
            .collect::<Vec<_>>();
        if let Some(uri) = matches.first() {
            println!(
                "hr: live source discovered by workspace scan for `fn {symbol}` and module {}",
                hint.join("::")
            );
            return Ok((*uri).clone());
        }
    }
    if candidates.len() == 1 {
        println!("hr: live source discovered by workspace scan for `fn {symbol}`");
        return Ok(candidates[0].clone());
    }
    if !candidates.is_empty() {
        return Err(format!(
            "ambiguous source for `{symbol}`; candidates: {}",
            candidates.join(", ")
        )
        .into());
    }

    if let Some(uri) = cargo_bin_source_uri(workspace_root, bin_name)? {
        let text = source_text_from_ra_or_disk(ra, &uri)?;
        if extract_function(&text, symbol).is_some() {
            return Ok(uri);
        }
    }

    Err(format!("could not discover source for function `{symbol}` in workspace").into())
}

fn scan_workspace_function_uris(
    workspace_root: &Path,
    symbol: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut matches = Vec::new();
    let mut stack = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries.into_iter().rev() {
            let path = entry.path();
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if path.is_dir() {
                if matches!(file_name, ".git" | "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                if extract_function(&source, symbol).is_some() {
                    matches.push(path_to_file_uri(&path));
                }
            }
        }
    }
    matches.sort();
    Ok(matches)
}

fn uri_matches_module_hint(workspace_root: &Path, uri: &str, module_hint: &[String]) -> bool {
    let Ok(path) = file_uri_to_path(uri) else {
        return false;
    };
    path_matches_module_hint(workspace_root, &path, module_hint)
}

fn path_matches_module_hint(workspace_root: &Path, path: &Path, module_hint: &[String]) -> bool {
    if module_hint.is_empty() {
        return true;
    }
    let Some(components) = source_module_components(workspace_root, path) else {
        return false;
    };
    components == module_hint
        || components.ends_with(module_hint)
        || module_hint.ends_with(&components)
        || module_hint.starts_with(&components)
}

fn source_module_components(workspace_root: &Path, path: &Path) -> Option<Vec<String>> {
    let relative = path.strip_prefix(workspace_root).ok()?;
    let mut parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    if parts.first().map(String::as_str) == Some("src") {
        parts.remove(0);
    }
    let last = parts.pop()?;
    if last == "mod.rs" {
        return Some(parts);
    }
    let stem = last.strip_suffix(".rs")?;
    parts.push(stem.to_string());
    Some(parts)
}

fn find_workspace_root(start: &Path) -> io::Result<PathBuf> {
    for dir in start.ancestors() {
        if dir.join("Cargo.toml").is_file() {
            return Ok(dir.to_path_buf());
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no Cargo.toml found from {}", start.display()),
    ))
}

fn cargo_command() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn merged_rustflags() -> OsString {
    let mut flags = std::env::var_os("RUSTFLAGS").unwrap_or_default();
    if !flags.is_empty() {
        flags.push(" ");
    }
    flags.push(PATCHABLE_ENTRY_FLAG);
    if let Some(codegen_units) = std::env::var_os(CODEGEN_UNITS_ENV)
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty())
    {
        flags.push(" ");
        flags.push(format!("-Ccodegen-units={codegen_units}"));
    }
    flags
}

fn merged_rustflags_string() -> String {
    merged_rustflags().to_string_lossy().into_owned()
}

struct RustAnalyzerSession {
    writer: Arc<Mutex<ChildStdin>>,
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
    state: SharedRaState,
}

impl RustAnalyzerSession {
    fn start(workspace_root: &Path) -> Result<Self, Box<dyn Error>> {
        let mut child = Command::new(rust_analyzer_command())
            .current_dir(workspace_root)
            .env("RA_LOG", "error")
            .env("RUSTC_BOOTSTRAP", "1")
            .env("RUSTFLAGS", merged_rustflags())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("failed to spawn rust-analyzer: {err}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or("failed to open rust-analyzer stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("failed to open rust-analyzer stdout")?;

        let writer = Arc::new(Mutex::new(stdin));
        let state = SharedRaState::default();
        let root = WorkspaceRoot::new(workspace_root)?;
        let config = rust_analyzer_config();
        let reader_writer = Arc::clone(&writer);
        let reader_root = root.clone();
        let reader_config = config.clone();
        let reader_state = state.clone();
        let reader = thread::spawn(move || {
            lsp_reader_loop(
                stdout,
                reader_writer,
                reader_state,
                reader_root,
                reader_config,
            );
        });

        send_lsp(
            &writer,
            json!({
                "jsonrpc": "2.0",
                "id": INITIALIZE_ID,
                "method": "initialize",
                "params": initialize_params(&root, &config),
            }),
        )?;
        wait_for_initialize(&state)?;
        send_lsp(
            &writer,
            json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {},
            }),
        )?;
        println!("hr: rust-analyzer initialized; requested server-side file watching");

        Ok(Self {
            writer,
            child,
            reader: Some(reader),
            state,
        })
    }

    fn activity_seq(&self) -> u64 {
        ra_activity_seq(&self.state)
    }

    fn wait_for_activity_after(
        &self,
        baseline: u64,
        duration: Duration,
    ) -> Result<Option<String>, Box<dyn Error>> {
        wait_for_activity_after(&self.state, baseline, duration)
    }

    fn wait_for_quiescent(&self, duration: Duration) -> Result<bool, Box<dyn Error>> {
        wait_for_quiescent(&self.state, duration)
    }

    fn wait_for_workspace_symbol(
        &self,
        query: &str,
        duration: Duration,
    ) -> Result<bool, Box<dyn Error>> {
        let deadline = SystemTime::now() + duration;
        loop {
            if self.workspace_symbol_contains(query)? {
                return Ok(true);
            }

            let now = SystemTime::now();
            if now >= deadline {
                return Ok(false);
            }
            let sleep_for = deadline
                .duration_since(now)
                .unwrap_or_default()
                .min(Duration::from_millis(750));
            thread::sleep(sleep_for);
        }
    }

    fn workspace_symbol_contains(&self, query: &str) -> Result<bool, Box<dyn Error>> {
        Ok(self.workspace_symbol_uri(query)?.is_some())
    }

    fn workspace_symbol_uri(&self, query: &str) -> Result<Option<String>, Box<dyn Error>> {
        reset_workspace_symbol_response(&self.state);
        send_lsp(
            &self.writer,
            json!({
                "jsonrpc": "2.0",
                "id": WORKSPACE_SYMBOL_ID,
                "method": "workspace/symbol",
                "params": {
                    "query": query,
                },
            }),
        )?;
        let symbols = wait_for_workspace_symbol_response(&self.state, Duration::from_secs(5))?;
        Ok(symbols
            .into_iter()
            .find(|symbol| symbol.name == query)
            .map(|symbol| symbol.uri))
    }

    fn view_file_text(&self, uri: &str) -> Result<String, Box<dyn Error>> {
        reset_file_text_response(&self.state);
        send_lsp(
            &self.writer,
            json!({
                "jsonrpc": "2.0",
                "id": VIEW_FILE_TEXT_ID,
                "method": "rust-analyzer/viewFileText",
                "params": {
                    "uri": uri,
                },
            }),
        )?;
        wait_for_file_text_response(&self.state, Duration::from_secs(5))
    }
}

fn maybe_hold_for_project_watch_proof(ra: &RustAnalyzerSession) -> Result<(), Box<dyn Error>> {
    let Some(raw) = std::env::var_os(WATCH_PROOF_ENV) else {
        return Ok(());
    };
    let seconds = raw
        .to_string_lossy()
        .parse::<u64>()
        .map_err(|err| format!("{WATCH_PROOF_ENV} must be seconds: {err}"))?;
    if seconds == 0 {
        return Ok(());
    }

    let duration = Duration::from_secs(seconds);
    println!(
        "hr: holding rust-analyzer workspace watcher for {seconds}s; edit any project source now"
    );
    let _ = ra.wait_for_quiescent(Duration::from_secs(20));
    let baseline = ra.activity_seq();
    let symbol = std::env::var(WATCH_PROOF_SYMBOL_ENV)
        .unwrap_or_else(|_| DEFAULT_WATCH_PROOF_SYMBOL.to_string());
    println!("hr: workspace-symbol proof query={symbol}");
    if ra.wait_for_workspace_symbol(&symbol, duration)? {
        println!("hr: project symbol observed after launch: {symbol}");
        let _ = ra.wait_for_quiescent(Duration::from_secs(20));
        return Ok(());
    }
    println!("hr: workspace symbol not observed yet; falling back to activity wait");

    match ra.wait_for_activity_after(baseline, duration)? {
        Some(reason) => {
            println!("hr: project activity observed after launch: {reason}");
            let _ = ra.wait_for_quiescent(Duration::from_secs(20));
        }
        None => {
            println!("hr: no rust-analyzer project activity observed during hold window");
        }
    }
    Ok(())
}

impl Drop for RustAnalyzerSession {
    fn drop(&mut self) {
        if !ra_is_quiescent(&self.state) {
            println!("hr: waiting for rust-analyzer to settle before shutdown");
            let _ = wait_for_quiescent(&self.state, Duration::from_secs(20));
        }

        let _ = send_lsp(
            &self.writer,
            json!({
                "jsonrpc": "2.0",
                "id": SHUTDOWN_ID,
                "method": "shutdown",
                "params": null,
            }),
        );
        let got_shutdown_ack =
            wait_for_shutdown_ack(&self.state, Duration::from_secs(2)).unwrap_or(false);
        if got_shutdown_ack {
            let _ = send_lsp(
                &self.writer,
                json!({
                    "jsonrpc": "2.0",
                    "method": "exit",
                    "params": null,
                }),
            );
        }

        for _ in 0..10 {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    if let Some(handle) = self.reader.take() {
                        let _ = handle.join();
                    }
                    return;
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct WorkspaceRoot {
    path: PathBuf,
    uri: String,
    name: String,
}

impl WorkspaceRoot {
    fn new(path: &Path) -> Result<Self, Box<dyn Error>> {
        let path = path.canonicalize()?;
        let uri = path_to_file_uri(&path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_string();
        Ok(Self { path, uri, name })
    }
}

#[derive(Clone, Default)]
struct SharedRaState(Arc<(Mutex<RaState>, Condvar)>);

#[derive(Debug, Default)]
struct RaState {
    initialized: bool,
    init_error: Option<String>,
    shutdown_ack: bool,
    reader_stopped: bool,
    last_health: Option<String>,
    last_quiescent: bool,
    activity_seq: u64,
    last_activity: Option<String>,
    symbol_response_ready: bool,
    symbol_response_error: Option<String>,
    symbol_response_symbols: Vec<WorkspaceSymbol>,
    file_text_ready: bool,
    file_text_error: Option<String>,
    file_text: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceSymbol {
    name: String,
    uri: String,
}

fn wait_for_initialize(state: &SharedRaState) -> Result<(), Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, Duration::from_secs(20), |state| {
            !state.initialized && state.init_error.is_none() && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;

    if guard.initialized {
        return Ok(());
    }
    if let Some(error) = &guard.init_error {
        return Err(format!("rust-analyzer initialize failed: {error}").into());
    }
    if guard.reader_stopped {
        return Err("rust-analyzer LSP reader stopped during initialize".into());
    }
    if timeout.timed_out() {
        return Err("timed out waiting for rust-analyzer initialize response".into());
    }

    Err("rust-analyzer initialize wait ended unexpectedly".into())
}

fn wait_for_shutdown_ack(
    state: &SharedRaState,
    duration: Duration,
) -> Result<bool, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, _timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.shutdown_ack && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    Ok(guard.shutdown_ack)
}

fn wait_for_quiescent(state: &SharedRaState, duration: Duration) -> Result<bool, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, _timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.last_quiescent && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    Ok(guard.last_quiescent || guard.reader_stopped)
}

fn wait_for_activity_after(
    state: &SharedRaState,
    baseline: u64,
    duration: Duration,
) -> Result<Option<String>, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            state.activity_seq <= baseline && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    if guard.activity_seq > baseline {
        Ok(guard.last_activity.clone())
    } else if timeout.timed_out() || guard.reader_stopped {
        Ok(None)
    } else {
        Ok(None)
    }
}

fn reset_workspace_symbol_response(state: &SharedRaState) {
    update_ra_state(state, |state| {
        state.symbol_response_ready = false;
        state.symbol_response_error = None;
        state.symbol_response_symbols.clear();
    });
}

fn wait_for_workspace_symbol_response(
    state: &SharedRaState,
    duration: Duration,
) -> Result<Vec<WorkspaceSymbol>, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.symbol_response_ready && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    if let Some(error) = &guard.symbol_response_error {
        return Err(format!("workspace/symbol failed: {error}").into());
    }
    if guard.symbol_response_ready {
        return Ok(guard.symbol_response_symbols.clone());
    }
    if timeout.timed_out() {
        return Err("timed out waiting for workspace/symbol response".into());
    }
    Err("rust-analyzer reader stopped before workspace/symbol response".into())
}

fn reset_file_text_response(state: &SharedRaState) {
    update_ra_state(state, |state| {
        state.file_text_ready = false;
        state.file_text_error = None;
        state.file_text = None;
    });
}

fn wait_for_file_text_response(
    state: &SharedRaState,
    duration: Duration,
) -> Result<String, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.file_text_ready && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    if let Some(error) = &guard.file_text_error {
        return Err(format!("rust-analyzer/viewFileText failed: {error}").into());
    }
    if guard.file_text_ready {
        return Ok(guard.file_text.clone().unwrap_or_default());
    }
    if timeout.timed_out() {
        return Err("timed out waiting for rust-analyzer/viewFileText response".into());
    }
    Err("rust-analyzer reader stopped before rust-analyzer/viewFileText response".into())
}

fn ra_activity_seq(state: &SharedRaState) -> u64 {
    let (lock, _) = &*state.0;
    lock.lock().map(|state| state.activity_seq).unwrap_or(0)
}

fn ra_is_quiescent(state: &SharedRaState) -> bool {
    let (lock, _) = &*state.0;
    lock.lock()
        .map(|state| state.last_quiescent || state.reader_stopped)
        .unwrap_or(true)
}

fn update_ra_state(state: &SharedRaState, update: impl FnOnce(&mut RaState)) {
    let (lock, cvar) = &*state.0;
    if let Ok(mut guard) = lock.lock() {
        update(&mut guard);
        cvar.notify_all();
    }
}

fn record_project_activity(state: &SharedRaState, reason: impl Into<String>) {
    let reason = reason.into();
    update_ra_state(state, |state| {
        state.activity_seq += 1;
        state.last_activity = Some(reason);
    });
}

fn log_ra_status(health: &str, quiescent: bool, message: Option<&str>) {
    if let Some(message) = message {
        println!("hr: ra status health={health} quiescent={quiescent} message={message}");
    } else {
        println!("hr: ra status health={health} quiescent={quiescent}");
    }
}

fn response_id(message: &Value) -> Option<i64> {
    let id = message.get("id")?;
    id.as_i64()
        .or_else(|| id.as_str().and_then(|value| value.parse().ok()))
}

fn response_error(message: &Value) -> Option<String> {
    message.get("error").map(ToString::to_string)
}

fn record_reader_stop(state: &SharedRaState) {
    update_ra_state(state, |state| {
        state.reader_stopped = true;
    });
}

fn record_initialize_response(state: &SharedRaState, message: &Value) {
    let error = response_error(message);
    update_ra_state(state, |state| {
        if let Some(error) = error {
            state.init_error = Some(error);
        } else {
            state.initialized = true;
        }
    });
}

fn record_workspace_symbol_response(state: &SharedRaState, message: &Value) {
    let error = response_error(message);
    let symbols = message
        .get("result")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name").and_then(Value::as_str)?;
                    let location = item.get("location")?;
                    let uri = location
                        .get("uri")
                        .or_else(|| location.get("targetUri"))
                        .and_then(Value::as_str)?;
                    Some(WorkspaceSymbol {
                        name: name.to_string(),
                        uri: uri.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    update_ra_state(state, |state| {
        state.symbol_response_ready = true;
        state.symbol_response_error = error;
        state.symbol_response_symbols = symbols;
    });
}

fn record_file_text_response(state: &SharedRaState, message: &Value) {
    let error = response_error(message);
    let text = message
        .get("result")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    update_ra_state(state, |state| {
        state.file_text_ready = true;
        state.file_text_error = error;
        state.file_text = text;
    });
}

fn rust_analyzer_command() -> OsString {
    std::env::var_os("HR_RUST_ANALYZER").unwrap_or_else(|| "rust-analyzer".into())
}

fn initialize_params(root: &WorkspaceRoot, config: &Value) -> Value {
    json!({
        "processId": std::process::id(),
        "rootPath": root.path,
        "rootUri": root.uri,
        "workspaceFolders": [{
            "uri": root.uri,
            "name": root.name,
        }],
        "capabilities": {
            "workspace": {
                "workspaceFolders": true,
                "configuration": true,
                "applyEdit": false,
                "symbol": {
                    "dynamicRegistration": false,
                },
                "didChangeWatchedFiles": {
                    "dynamicRegistration": false,
                    "relativePatternSupport": true,
                },
                "inlayHint": {
                    "refreshSupport": false,
                },
                "semanticTokens": {
                    "refreshSupport": false,
                },
            },
            "window": {
                "workDoneProgress": true,
                "showMessage": {
                    "messageActionItem": {
                        "additionalPropertiesSupport": false,
                    },
                },
            },
            "textDocument": {
                "synchronization": {
                    "dynamicRegistration": false,
                    "didSave": true,
                    "willSave": false,
                    "willSaveWaitUntil": false,
                },
                "publishDiagnostics": {
                    "relatedInformation": true,
                    "dataSupport": true,
                },
            },
            "experimental": {
                "serverStatusNotification": true,
            },
        },
        "initializationOptions": config,
    })
}

fn rust_analyzer_config() -> Value {
    json!({
        "files": {
            "watcher": "server",
        },
        "cargo": {
            "autoreload": true,
            "extraEnv": {
                "RUSTC_BOOTSTRAP": "1",
                "RUSTFLAGS": merged_rustflags_string(),
            },
        },
        "checkOnSave": true,
        "check": {
            "allTargets": false,
            "extraEnv": {
                "RUSTC_BOOTSTRAP": "1",
                "RUSTFLAGS": merged_rustflags_string(),
            },
        },
        "diagnostics": {
            "enable": true,
        },
        "server": {
            "extraEnv": {
                "RUSTC_BOOTSTRAP": "1",
                "RUSTFLAGS": merged_rustflags_string(),
            },
        },
    })
}

fn lsp_reader_loop(
    stdout: std::process::ChildStdout,
    writer: Arc<Mutex<ChildStdin>>,
    state: SharedRaState,
    root: WorkspaceRoot,
    config: Value,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_lsp(&mut reader) {
            Ok(Some(message)) => handle_lsp_message(message, &writer, &state, &root, &config),
            Ok(None) => {
                record_reader_stop(&state);
                break;
            }
            Err(err) => {
                println!("hr: ra reader stopped: {err}");
                record_reader_stop(&state);
                break;
            }
        }
    }
}

fn handle_lsp_message(
    message: Value,
    writer: &Arc<Mutex<ChildStdin>>,
    state: &SharedRaState,
    root: &WorkspaceRoot,
    config: &Value,
) {
    if message.get("method").is_none() {
        match response_id(&message) {
            Some(INITIALIZE_ID) => record_initialize_response(state, &message),
            Some(SHUTDOWN_ID) => update_ra_state(state, |state| {
                state.shutdown_ack = true;
            }),
            Some(WORKSPACE_SYMBOL_ID) => record_workspace_symbol_response(state, &message),
            Some(VIEW_FILE_TEXT_ID) => record_file_text_response(state, &message),
            _ => {}
        }
        return;
    }

    if let (Some(method), Some(id)) = (
        message.get("method").and_then(Value::as_str),
        message.get("id").cloned(),
    ) {
        let result = match method {
            "workspace/configuration" => workspace_configuration_result(&message, config),
            "workspace/workspaceFolders" => json!([{
                "uri": root.uri,
                "name": root.name,
            }]),
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest" => Value::Null,
            "workspace/applyEdit" => json!({ "applied": false }),
            _ => Value::Null,
        };
        let _ = send_response(writer, id, result);
        return;
    }

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    match method {
        "experimental/serverStatus" => {
            let params = message.get("params").unwrap_or(&Value::Null);
            let health = params
                .get("health")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let quiescent = params
                .get("quiescent")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = params
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            update_ra_state(state, |state| {
                state.last_health = Some(health.clone());
                state.last_quiescent = quiescent;
            });
            record_project_activity(
                state,
                format!("serverStatus health={health} quiescent={quiescent}"),
            );
            log_ra_status(&health, quiescent, message.as_deref());
        }
        "textDocument/publishDiagnostics" => {
            let params = message.get("params").unwrap_or(&Value::Null);
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_string();
            let count = params
                .get("diagnostics")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            record_project_activity(state, format!("diagnostics count={count} uri={uri}"));
            println!("hr: ra diagnostics {count} {uri}");
        }
        "window/logMessage" | "window/showMessage" => {
            if let Some(text) = message
                .get("params")
                .and_then(|params| params.get("message"))
                .and_then(Value::as_str)
            {
                println!("hr: ra {text}");
            }
        }
        _ => {}
    }
}

fn workspace_configuration_result(message: &Value, config: &Value) -> Value {
    let item_count = message
        .get("params")
        .and_then(|params| params.get("items"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(1);
    Value::Array((0..item_count).map(|_| config.clone()).collect())
}

fn send_response(
    writer: &Arc<Mutex<ChildStdin>>,
    id: Value,
    result: Value,
) -> Result<(), Box<dyn Error>> {
    send_lsp(
        writer,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
}

fn send_lsp(writer: &Arc<Mutex<ChildStdin>>, value: Value) -> Result<(), Box<dyn Error>> {
    let body = serde_json::to_vec(&value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut writer = writer
        .lock()
        .map_err(|_| "rust-analyzer stdin lock poisoned")?;
    writer.write_all(header.as_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_lsp(reader: &mut BufReader<std::process::ChildStdout>) -> io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some(raw) = line.strip_prefix("Content-Length:") {
            let len = raw
                .trim()
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            content_length = Some(len);
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing Content-Length in LSP frame",
        )
    })?;
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid LSP JSON body: {err}"),
        )
    })
}

fn path_to_file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let text = path.to_string_lossy();
    if !text.starts_with('/') {
        uri.push('/');
    }
    for byte in text.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(*byte as char)
            }
            other => {
                let _ = fmt::write(&mut uri, format_args!("%{other:02X}"));
            }
        }
    }
    uri
}
