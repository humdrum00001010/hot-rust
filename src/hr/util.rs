use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{CODEGEN_UNITS_ENV, PATCHABLE_ENTRY_FLAG, TIMING_ENV};

pub(crate) fn log_timing(label: &str, start: Instant) {
    if env_flag(TIMING_ENV) {
        println!(
            "hr: timing {label} elapsed={:.3}s",
            start.elapsed().as_secs_f64()
        );
    }
}

pub(crate) fn file_uri_to_path(uri: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
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

pub(crate) fn dylib_filename(stem: &str) -> String {
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

pub(crate) fn find_workspace_root(start: &Path) -> io::Result<PathBuf> {
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

pub(crate) fn cargo_command() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

pub(crate) fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub(crate) fn merged_rustflags() -> OsString {
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

pub(crate) fn merged_rustflags_string() -> String {
    merged_rustflags().to_string_lossy().into_owned()
}
