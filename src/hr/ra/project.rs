use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::Path;

use super::path_to_file_uri;
use crate::rust_source::{
    containing_impl_type, extract_function_at, function_item_positions, function_name_at,
    looks_like_function_item, ParsedFunction,
};

#[derive(Clone)]
pub(crate) struct ProjectFunction {
    pub(crate) source_uri: String,
    pub(crate) name: String,
    pub(crate) impl_type: Option<String>,
    pub(crate) module_components: Vec<String>,
    pub(crate) function: ParsedFunction,
}

pub(crate) struct ProjectSnapshot {
    functions: BTreeMap<FunctionKey, ProjectFunction>,
    file_count: usize,
}

pub(crate) enum ProjectDiff {
    NoChange,
    BodyOnly(ProjectFunction),
    RebuildRequired(String),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FunctionKey {
    source_uri: String,
    name: String,
    impl_type: Option<String>,
    ordinal: usize,
}

impl ProjectSnapshot {
    pub(crate) fn scan(workspace_root: &Path) -> Result<Self, Box<dyn Error>> {
        let mut functions = BTreeMap::new();
        let mut file_count = 0usize;
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
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(source) = fs::read_to_string(&path) else {
                    continue;
                };
                file_count += 1;
                collect_file_functions(workspace_root, &path, &source, &mut functions);
            }
        }
        Ok(Self {
            functions,
            file_count,
        })
    }

    pub(crate) fn function_count(&self) -> usize {
        self.functions.len()
    }

    pub(crate) fn file_count(&self) -> usize {
        self.file_count
    }

    pub(crate) fn diff(&self, next: &Self) -> ProjectDiff {
        let mut body_changes = Vec::new();
        let mut structural = Vec::new();

        for (key, old) in &self.functions {
            let Some(new) = next.functions.get(key) else {
                structural.push(format!("function removed {}", old.label()));
                continue;
            };
            if old.function.signature != new.function.signature {
                structural.push(format!("signature changed {}", old.label()));
                continue;
            }
            if old.function.body != new.function.body {
                body_changes.push(new.clone());
            }
        }

        for (key, new) in &next.functions {
            if !self.functions.contains_key(key) {
                structural.push(format!("function added {}", new.label()));
            }
        }

        if !structural.is_empty() {
            return ProjectDiff::RebuildRequired(structural.join("; "));
        }
        match body_changes.len() {
            0 => ProjectDiff::NoChange,
            1 => ProjectDiff::BodyOnly(body_changes.remove(0)),
            count => ProjectDiff::RebuildRequired(format!(
                "{count} function bodies changed; automatic multi-patch is not wired yet"
            )),
        }
    }
}

impl ProjectFunction {
    pub(crate) fn label(&self) -> String {
        match &self.impl_type {
            Some(impl_type) => format!("{}::{}", impl_type, self.name),
            None if self.module_components.is_empty() => self.name.clone(),
            None => format!("{}::{}", self.module_components.join("::"), self.name),
        }
    }

    pub(crate) fn symbol_impl_component(&self) -> Option<String> {
        self.impl_type
            .as_deref()
            .and_then(symbol_impl_component)
            .map(ToOwned::to_owned)
    }
}

fn collect_file_functions(
    workspace_root: &Path,
    path: &Path,
    source: &str,
    functions: &mut BTreeMap<FunctionKey, ProjectFunction>,
) {
    let source_uri = path_to_file_uri(path);
    let module_components = source_module_components(workspace_root, path).unwrap_or_default();
    let mut ordinals = BTreeMap::<(String, Option<String>), usize>::new();
    for fn_start in function_item_positions(source) {
        if !looks_like_function_item(source, fn_start) {
            continue;
        }
        let Some(name) = function_name_at(source, fn_start) else {
            continue;
        };
        let Some(function) = extract_function_at(source, fn_start) else {
            continue;
        };
        let impl_type = containing_impl_type(source, function.signature_start);
        let ordinal_key = (name.clone(), impl_type.clone());
        let ordinal = ordinals.entry(ordinal_key).or_default();
        let key = FunctionKey {
            source_uri: source_uri.clone(),
            name: name.clone(),
            impl_type: impl_type.clone(),
            ordinal: *ordinal,
        };
        *ordinal += 1;
        functions.insert(
            key,
            ProjectFunction {
                source_uri: source_uri.clone(),
                name,
                impl_type,
                module_components: module_components.clone(),
                function,
            },
        );
    }
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

fn symbol_impl_component(impl_type: &str) -> Option<&str> {
    let without_generics = impl_type.split('<').next().unwrap_or(impl_type).trim();
    without_generics
        .rsplit("::")
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn diff_detects_one_body_change() {
        let root = temp_project_root();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct App;\nimpl App {\n    pub fn tick(&self) -> i32 { 1 }\n}\n",
        )
        .unwrap();
        let before = ProjectSnapshot::scan(&root).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct App;\nimpl App {\n    pub fn tick(&self) -> i32 { 2 }\n}\n",
        )
        .unwrap();
        let after = ProjectSnapshot::scan(&root).unwrap();

        match before.diff(&after) {
            ProjectDiff::BodyOnly(function) => assert_eq!(function.label(), "App::tick"),
            _ => panic!("expected body-only change"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn diff_rejects_signature_change() {
        let root = temp_project_root();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn value() -> i32 { 1 }\n").unwrap();
        let before = ProjectSnapshot::scan(&root).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn value(v: i32) -> i32 { v }\n",
        )
        .unwrap();
        let after = ProjectSnapshot::scan(&root).unwrap();

        match before.diff(&after) {
            ProjectDiff::RebuildRequired(reason) => assert!(reason.contains("signature changed")),
            _ => panic!("expected rebuild-required change"),
        }
        let _ = fs::remove_dir_all(root);
    }

    fn temp_project_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("hot-rust-project-snapshot-test-{nonce}"))
    }
}
