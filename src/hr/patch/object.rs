//! Diagnostic object/CGU probe backends.
//!
//! This module is retained for rustc-output experiments. The normal service path
//! uses shadow fake-crate dylibs; exact-function object installation is not the
//! active path.

use serde_json::Value;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::super::rust_source::{
    containing_impl_type, function_params_open, matching_paren, method_receiver, patch_signature,
    split_top_level_commas, ParsedFunction,
};
use super::super::util::{cargo_command, file_uri_to_path, merged_rustflags};

pub(super) struct ObjectProbe {
    root: PathBuf,
    object: Option<PathBuf>,
    elapsed: Duration,
    status: String,
    symbols: Vec<String>,
    relocations: Vec<String>,
    compiler_notes: Vec<String>,
}

impl ObjectProbe {
    pub(super) fn report(&self) {
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

pub(crate) struct CguProbe {
    target_dir: PathBuf,
    before: Option<CguObject>,
    pub(crate) after: Option<CguObject>,
    elapsed: Duration,
    status: String,
    compiler_notes: Vec<String>,
}

impl CguProbe {
    pub(crate) fn report(&self) {
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
pub(crate) struct CguObject {
    pub(crate) path: PathBuf,
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

pub(crate) fn build_incremental_cgu_probe(
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

pub(super) fn build_function_patch_object_probe(
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

pub(super) fn method_object_probe_signature(
    patch_symbol: &str,
    impl_path: &str,
    signature: &str,
) -> Result<String, Box<dyn Error>> {
    let open = function_params_open(signature).ok_or("method signature has no `(`")?;
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
