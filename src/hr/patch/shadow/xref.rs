//! Source xref indexes for shadow-fake stub generation.
//!
//! This cache replaced repeated full-tree scans in the hot path. It is active
//! for `ShadowFake`, while direct uncached scans remain only as fallbacks.

use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use super::super::super::rust_source::{
    containing_impl_type, extract_function_at, function_item_has_no_body, function_item_positions,
    function_name_at, function_name_from_signature, looks_like_function_item, method_receiver,
    ParsedFunction,
};

#[derive(Debug, Clone)]
pub(super) struct MethodDefinition {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) impl_type: String,
}

#[derive(Debug, Clone)]
pub(super) struct FunctionDefinition {
    pub(super) name: String,
    pub(super) path: PathBuf,
    pub(super) module_components: Vec<String>,
}

#[derive(Default)]
pub(crate) struct ShadowXrefCache {
    method_indexes: HashMap<PathBuf, HashMap<String, Vec<MethodDefinition>>>,
    function_indexes: HashMap<PathBuf, HashMap<String, Vec<FunctionDefinition>>>,
}

impl ShadowXrefCache {
    pub(super) fn method_index(
        &mut self,
        root: &Path,
    ) -> Result<&HashMap<String, Vec<MethodDefinition>>, Box<dyn Error>> {
        let key = cache_root_key(root);
        if !self.method_indexes.contains_key(&key) {
            let index = method_definitions_by_name(root)?;
            println!(
                "hr: shadow-fake cached method xrefs entries={}",
                xref_entry_count(&index)
            );
            self.method_indexes.insert(key.clone(), index);
        }
        Ok(self
            .method_indexes
            .get(&key)
            .ok_or("method xref cache insert failed")?)
    }

    pub(super) fn function_index(
        &mut self,
        root: &Path,
    ) -> Result<&HashMap<String, Vec<FunctionDefinition>>, Box<dyn Error>> {
        let key = cache_root_key(root);
        if !self.function_indexes.contains_key(&key) {
            let index = function_definitions_by_name(root)?;
            println!(
                "hr: shadow-fake cached function xrefs entries={}",
                xref_entry_count(&index)
            );
            self.function_indexes.insert(key.clone(), index);
        }
        Ok(self
            .function_indexes
            .get(&key)
            .ok_or("function xref cache insert failed")?)
    }

    pub(super) fn prewarm(&mut self, root: &Path) -> Result<(), Box<dyn Error>> {
        let _ = self.method_index(root)?.len();
        let _ = self.function_index(root)?.len();
        Ok(())
    }
}
pub(super) fn method_definitions_by_name(
    root: &Path,
) -> Result<HashMap<String, Vec<MethodDefinition>>, Box<dyn Error>> {
    let src = root.join("src");
    let mut stack = vec![src];
    let mut out: HashMap<String, Vec<MethodDefinition>> = HashMap::new();
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
            for fn_start in function_item_positions(&text) {
                if !looks_like_function_item(&text, fn_start) {
                    continue;
                }
                if function_item_has_no_body(&text, fn_start) {
                    continue;
                }
                let Some(name) = function_name_at(&text, fn_start) else {
                    continue;
                };
                let Some(function) = extract_function_at(&text, fn_start) else {
                    continue;
                };
                if method_receiver(&function.signature).is_none() {
                    continue;
                }
                let Some(impl_type) = containing_impl_type(&text, function.signature_start) else {
                    continue;
                };
                out.entry(name).or_default().push(MethodDefinition {
                    name: function_name_from_signature(&function.signature)
                        .unwrap_or_else(|| function_name_at(&text, fn_start).unwrap_or_default()),
                    path: path.clone(),
                    impl_type,
                });
            }
        }
    }
    Ok(out)
}

pub(super) fn function_definitions_by_name(
    root: &Path,
) -> Result<HashMap<String, Vec<FunctionDefinition>>, Box<dyn Error>> {
    let src = root.join("src");
    let mut stack = vec![src];
    let mut out: HashMap<String, Vec<FunctionDefinition>> = HashMap::new();
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
            let module_components = rust_module_components_for_source(root, &path);
            for fn_start in function_item_positions(&text) {
                if !looks_like_function_item(&text, fn_start) {
                    continue;
                }
                if function_item_has_no_body(&text, fn_start) {
                    continue;
                }
                let Some(name) = function_name_at(&text, fn_start) else {
                    continue;
                };
                let Some(function) = extract_function_at(&text, fn_start) else {
                    continue;
                };
                if method_receiver(&function.signature).is_some() {
                    continue;
                }
                out.entry(name).or_default().push(FunctionDefinition {
                    name: function_name_from_signature(&function.signature)
                        .unwrap_or_else(|| function_name_at(&text, fn_start).unwrap_or_default()),
                    path: path.clone(),
                    module_components: module_components.clone(),
                });
            }
        }
    }
    Ok(out)
}

pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn cache_root_key(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn xref_entry_count<T>(index: &HashMap<String, Vec<T>>) -> usize {
    index.values().map(Vec::len).sum()
}
pub(super) fn extract_method_definition(
    text: &str,
    method_name: &str,
    impl_type: &str,
) -> Option<ParsedFunction> {
    for fn_start in function_item_positions(text) {
        if !looks_like_function_item(text, fn_start) || function_item_has_no_body(text, fn_start) {
            continue;
        }
        if function_name_at(text, fn_start).as_deref() != Some(method_name) {
            continue;
        }
        let function = extract_function_at(text, fn_start)?;
        if method_receiver(&function.signature).is_none() {
            continue;
        }
        if containing_impl_type(text, function.signature_start).as_deref() == Some(impl_type) {
            return Some(function);
        }
    }
    None
}

pub(super) fn extract_free_function_definition(
    text: &str,
    function_name: &str,
) -> Option<ParsedFunction> {
    for fn_start in function_item_positions(text) {
        if !looks_like_function_item(text, fn_start) || function_item_has_no_body(text, fn_start) {
            continue;
        }
        if function_name_at(text, fn_start).as_deref() != Some(function_name) {
            continue;
        }
        let function = extract_function_at(text, fn_start)?;
        if method_receiver(&function.signature).is_none() {
            return Some(function);
        }
    }
    None
}
fn rust_module_components_for_source(root: &Path, path: &Path) -> Vec<String> {
    let src = root.join("src");
    let rel = path.strip_prefix(&src).unwrap_or(path);
    let mut components = Vec::new();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            let Some(component) = component.as_os_str().to_str() else {
                continue;
            };
            if !component.is_empty() {
                components.push(component.to_string());
            }
        }
    }
    if let Some(stem) = rel.file_stem().and_then(|stem| stem.to_str()) {
        if !matches!(stem, "lib" | "main" | "mod") {
            components.push(stem.to_string());
        }
    }
    components
}
