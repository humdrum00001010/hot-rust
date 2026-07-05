//! Fake shadow-crate filesystem helpers.
//!
//! These are implementation support for the shadow backend, not independent
//! patch strategies. They are intentionally separated from the hot orchestration
//! code because most functions here are tree-copy/prune experiment machinery.

use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

use super::super::super::rust_source::{prune_function_bodies_in_source, split_top_level_commas};
use super::super::super::util::dylib_filename;
use super::super::super::SHADOW_PRESERVE_ENV;

pub(super) struct ShadowPruneReport {
    pub(super) files_touched: usize,
    pub(super) functions_pruned: usize,
    pub(super) preserve_prefixes: Vec<String>,
    pub(super) pruned_preserved: bool,
}

pub(super) struct ShadowSerdeStripReport {
    pub(super) files_touched: usize,
    pub(super) derive_entries_removed: usize,
    pub(super) serde_attrs_removed: usize,
}

pub(super) struct PersistentShadowCrate {
    pub(super) root: PathBuf,
    pub(super) lib_name: String,
}

pub(super) struct ShadowSyncReport {
    pub(super) files_copied: usize,
    pub(super) bytes_copied: u64,
}
pub(super) fn persistent_shadow_crate(
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

pub(super) fn fake_shadow_crate_ready(root: &Path) -> bool {
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

pub(super) fn sync_shadow_tree_changed(src: &Path, dst: &Path) -> io::Result<ShadowSyncReport> {
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

pub(super) fn copy_unique_patch_dylib(
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

pub(super) fn write_fake_crate_root(root: &Path) -> io::Result<()> {
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
pub(super) fn prune_shadow_function_bodies(
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

pub(super) fn strip_fake_serde_derives(
    root: &Path,
) -> Result<ShadowSerdeStripReport, Box<dyn Error>> {
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
pub(super) fn copy_workspace_for_shadow(src: &Path, dst: &Path) -> io::Result<()> {
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

pub(super) fn copy_workspace_for_fake_shadow(src: &Path, dst: &Path) -> io::Result<()> {
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

pub(super) fn rewrite_shadow_manifest(manifest: &Path, lib_name: &str) -> io::Result<()> {
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
