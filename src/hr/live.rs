//! Live patch source/artifact/runtime helpers.
//!
//! This module does not own the edit loop. `RustAnalyzerDriver` receives RA
//! activity and calls these helpers to discover source, build patches, and send
//! runtime RPCs.

use serde_json::{json, Value};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use super::patch::{
    build_function_patch_dylib, prewarm_shadow_xrefs_if_ready, BuiltLivePatch, ShadowXrefCache,
};
use super::ra::{path_to_file_uri, RustAnalyzerSession};
use super::rust_source::extract_function;
use super::session::HotSession;
use super::symbols::{source_path_hint_from_symbol, BinarySymbolResolver};
use super::util::{cargo_command, file_uri_to_path, log_timing, merged_rustflags};
use super::{LIVE_SYMBOL_ENV, RUNTIME_DYLIB_ENV};

pub(crate) fn build_live_patch_once(
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
    let mut xref_cache = ShadowXrefCache::default();
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
    prewarm_shadow_xrefs_if_ready(workspace_root, &live.symbol, &mut xref_cache)?;
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
        Some(&mut xref_cache),
    )?;
    log_timing("build-only-patch-build", start);
    println!("hr: patch build-only completed");
    log_timing("build-only-total", total_start);
    Ok(())
}

pub(crate) struct LiveConfig {
    pub(crate) symbol: String,
    pub(crate) patch_symbol: String,
    pub(crate) runtime_dylib: PathBuf,
}

impl LiveConfig {
    pub(crate) fn from_env() -> Result<Option<Self>, Box<dyn Error>> {
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

    pub(crate) fn apply_runtime_env(&self, command: &mut Command) -> Result<(), Box<dyn Error>> {
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

pub(crate) fn source_snippet(source: &str, symbol: &str) -> String {
    let Some(pos) = source.find(symbol) else {
        return "<symbol not found>".to_string();
    };
    let start = pos.saturating_sub(80);
    let end = (pos + 160).min(source.len());
    source[start..end].replace('\n', "\\n")
}

pub(crate) fn source_text_from_ra_or_disk(
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

pub(crate) fn send_patch_command(
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

pub(crate) fn send_object_patch_command(
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

pub(crate) fn discover_live_source_uri(
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
