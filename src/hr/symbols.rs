use serde_json::Value;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::util::{cargo_command, merged_rustflags};

pub(crate) struct BinarySymbolResolver {
    executable: PathBuf,
    candidates: Vec<SymbolCandidate>,
}

impl BinarySymbolResolver {
    pub(crate) fn load(workspace_root: &Path, executable: &Path) -> Result<Self, Box<dyn Error>> {
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

    pub(crate) fn symbol_for(&self, source_symbol: &str) -> Result<String, Box<dyn Error>> {
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

    pub(crate) fn symbol_for_method(
        &self,
        impl_type: &str,
        source_symbol: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut candidates = self
            .candidates
            .iter()
            .filter(|candidate| candidate.symbol.contains(source_symbol))
            .map(|candidate| {
                let exact_component = candidate
                    .components
                    .iter()
                    .any(|component| component == source_symbol);
                let impl_match = candidate
                    .components
                    .iter()
                    .any(|component| component == impl_type);
                (candidate, exact_component, impl_match)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(candidate, exact_component, impl_match)| {
            (
                !candidate.crate_match,
                !*impl_match,
                !*exact_component,
                candidate.symbol.len(),
                candidate.symbol.clone(),
            )
        });
        let Some((candidate, _, _)) = candidates.first() else {
            return Err(format!(
                "could not find text symbol containing `{source_symbol}` in {}",
                self.executable.display()
            )
            .into());
        };
        Ok(candidate.symbol.clone())
    }

    pub(crate) fn symbol_for_function(
        &self,
        module_components: &[String],
        source_symbol: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut candidates = self
            .candidates
            .iter()
            .filter(|candidate| candidate.symbol.contains(source_symbol))
            .map(|candidate| {
                let exact_component = candidate
                    .components
                    .iter()
                    .any(|component| component == source_symbol);
                let missing_module_components = module_components
                    .iter()
                    .filter(|component| {
                        !candidate
                            .components
                            .iter()
                            .any(|candidate_component| candidate_component == *component)
                    })
                    .count();
                (candidate, exact_component, missing_module_components)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(candidate, exact_component, missing_module_components)| {
            (
                !candidate.crate_match,
                *missing_module_components,
                !*exact_component,
                candidate.symbol.len(),
                candidate.symbol.clone(),
            )
        });
        let Some((candidate, _, _)) = candidates.first() else {
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

pub(crate) fn source_path_hint_from_symbol(
    runtime_symbol: &str,
    source_symbol: &str,
) -> Option<Vec<String>> {
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
