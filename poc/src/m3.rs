//! M3: prove the rust-analyzer-side change oracle.
//!
//! This is the first executable slice of the watcher/oracle. It uses
//! rust-analyzer's syntax crate to parse two versions of a Rust file, recover
//! item identity, and route edits:
//!   - function body changed, same signature and no structural changes -> patch
//!   - function signature/item shape/struct layout changed -> rebuild
//!   - parse/validation errors -> wait
//!
//! This is deliberately not codegen or patching. M1/M2 already prove the patcher.
//! M3 proves the decision boundary that tells the driver whether invoking M2 is
//! safe.

use ra_ap_ide::{Analysis, AssistResolveStrategy, Diagnostic, DiagnosticsConfig, Severity};
use ra_ap_paths::AbsPathBuf;
use ra_ap_syntax::ast::{self, HasName};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxError, SyntaxNode, TextRange};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use triomphe::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    NoChange,
    BodyOnly { targets: Vec<PatchTarget> },
    Structural { reasons: Vec<String> },
    Invalid { errors: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchTarget {
    path: String,
    export_symbol: String,
}

#[derive(Debug, Clone)]
struct Snapshot {
    functions: BTreeMap<String, FunctionInfo>,
    structs: BTreeMap<String, StructInfo>,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    signature: String,
    body: String,
}

#[derive(Debug, Clone)]
struct StructInfo {
    shape: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let base = r#"
pub struct Layout {
    pub width: u32,
}

pub mod render {
    pub fn paint(input: u32) -> u32 {
        input + 1
    }

    pub fn stable() -> u32 {
        7
    }
}
"#;

    let body_edit = r#"
pub struct Layout {
    pub width: u32,
}

pub mod render {
    pub fn paint(input: u32) -> u32 {
        input + 2
    }

    pub fn stable() -> u32 {
        7
    }
}
"#;

    let signature_edit = r#"
pub struct Layout {
    pub width: u32,
}

pub mod render {
    pub fn paint(input: u64) -> u32 {
        input as u32 + 2
    }

    pub fn stable() -> u32 {
        7
    }
}
"#;

    let struct_edit = r#"
pub struct Layout {
    pub width: u32,
    pub height: u32,
}

pub mod render {
    pub fn paint(input: u32) -> u32 {
        input + 2
    }

    pub fn stable() -> u32 {
        7
    }
}
"#;

    let invalid_edit = r#"
pub struct Layout {
    pub width: u32,
}

pub mod render {
    pub fn paint(input: u32 -> u32 {
        input + 2
    }
}
"#;

    let type_error_edit = r#"
pub struct Layout {
    pub width: u32,
}

pub mod render {
    pub fn paint(input: u32) -> u32 {
        true
    }

    pub fn stable() -> u32 {
        7
    }
}
"#;

    let cases = [
        ("no_change", base, base, RouteExpectation::NoChange),
        (
            "body_only",
            base,
            body_edit,
            RouteExpectation::BodyOnly("render::paint"),
        ),
        (
            "signature_change",
            base,
            signature_edit,
            RouteExpectation::Structural("signature changed for render::paint"),
        ),
        (
            "struct_layout_change",
            base,
            struct_edit,
            RouteExpectation::Structural("struct shape changed for Layout"),
        ),
        (
            "invalid_source",
            base,
            invalid_edit,
            RouteExpectation::Invalid,
        ),
        (
            "semantic_error",
            base,
            type_error_edit,
            RouteExpectation::Invalid,
        ),
    ];

    for (name, old_src, new_src, expected) in cases {
        let route = classify_edit(old_src, new_src);
        println!("{name}: {route}");
        expected.assert_matches(&route);
    }

    println!("OK: M3 oracle routes body-only edits to patch, structural edits to rebuild, and invalid edits to wait.");
    Ok(())
}

fn classify_edit(old_src: &str, new_src: &str) -> Route {
    let old_snapshot = match Snapshot::parse(old_src) {
        Ok(snapshot) => snapshot,
        Err(errors) => return Route::Invalid { errors },
    };
    let new_snapshot = match Snapshot::parse(new_src) {
        Ok(snapshot) => snapshot,
        Err(errors) => return Route::Invalid { errors },
    };

    let errors = semantic_errors(new_src);
    if !errors.is_empty() {
        return Route::Invalid { errors };
    }

    let mut structural = Vec::new();

    for path in union_keys(&old_snapshot.structs, &new_snapshot.structs) {
        match (
            old_snapshot.structs.get(&path),
            new_snapshot.structs.get(&path),
        ) {
            (Some(old), Some(new)) if old.shape != new.shape => {
                structural.push(format!("struct shape changed for {path}"));
            }
            (Some(_), None) => structural.push(format!("struct removed: {path}")),
            (None, Some(_)) => structural.push(format!("struct added: {path}")),
            _ => {}
        }
    }

    let mut body_targets = Vec::new();
    for path in union_keys(&old_snapshot.functions, &new_snapshot.functions) {
        match (
            old_snapshot.functions.get(&path),
            new_snapshot.functions.get(&path),
        ) {
            (Some(old), Some(new)) if old.signature != new.signature => {
                structural.push(format!("signature changed for {path}"));
            }
            (Some(old), Some(new)) if old.body != new.body => {
                body_targets.push(PatchTarget {
                    export_symbol: export_symbol_for(&path),
                    path,
                });
            }
            (Some(_), None) => structural.push(format!("function removed: {path}")),
            (None, Some(_)) => structural.push(format!("function added: {path}")),
            _ => {}
        }
    }

    if !structural.is_empty() {
        return Route::Structural {
            reasons: structural,
        };
    }

    if !body_targets.is_empty() {
        return Route::BodyOnly {
            targets: body_targets,
        };
    }

    Route::NoChange
}

impl Snapshot {
    fn parse(src: &str) -> Result<Self, Vec<String>> {
        let parse = SourceFile::parse(src, Edition::CURRENT);
        let errors = parse_errors(parse.errors());
        if !errors.is_empty() {
            return Err(errors);
        }

        let tree = parse.tree();
        let root = tree.syntax();
        let mut functions = BTreeMap::new();
        let mut structs = BTreeMap::new();

        for node in root.descendants() {
            if let Some(function) = ast::Fn::cast(node.clone()) {
                if let Some(path) = item_path(function.syntax()) {
                    let body = function
                        .body()
                        .map(|body| body.syntax().text().to_string())
                        .unwrap_or_default();
                    let signature = function
                        .body()
                        .map(|body| {
                            remove_child_range(
                                &function.syntax().text().to_string(),
                                function.syntax().text_range(),
                                body.syntax().text_range(),
                            )
                        })
                        .unwrap_or_else(|| function.syntax().text().to_string());

                    functions.insert(path, FunctionInfo { signature, body });
                }
            } else if let Some(struct_item) = ast::Struct::cast(node) {
                if let Some(path) = item_path(struct_item.syntax()) {
                    structs.insert(
                        path,
                        StructInfo {
                            shape: struct_item.syntax().text().to_string(),
                        },
                    );
                }
            }
        }

        Ok(Self { functions, structs })
    }
}

fn item_path(node: &SyntaxNode) -> Option<String> {
    let mut pieces = Vec::new();

    for ancestor in node.ancestors() {
        if let Some(function) = ast::Fn::cast(ancestor.clone()) {
            if function.syntax() == node {
                pieces.push(function.name()?.text().to_string());
            }
        } else if let Some(struct_item) = ast::Struct::cast(ancestor.clone()) {
            if struct_item.syntax() == node {
                pieces.push(struct_item.name()?.text().to_string());
            }
        } else if let Some(module) = ast::Module::cast(ancestor) {
            if module.item_list().is_some() {
                pieces.push(module.name()?.text().to_string());
            }
        }
    }

    if pieces.is_empty() {
        None
    } else {
        pieces.reverse();
        Some(pieces.join("::"))
    }
}

fn remove_child_range(
    parent_text: &str,
    parent_range: TextRange,
    child_range: TextRange,
) -> String {
    let parent_start = u32::from(parent_range.start()) as usize;
    let start = u32::from(child_range.start()) as usize - parent_start;
    let end = u32::from(child_range.end()) as usize - parent_start;

    let mut text = String::with_capacity(parent_text.len() - (end - start));
    text.push_str(&parent_text[..start]);
    text.push_str(&parent_text[end..]);
    text
}

fn parse_errors(errors: Vec<SyntaxError>) -> Vec<String> {
    errors.into_iter().map(|error| format!("{error}")).collect()
}

fn semantic_errors(src: &str) -> Vec<String> {
    let cwd = AbsPathBuf::try_from("/").expect("diagnostics cwd must be absolute");
    let (analysis, file_id) = Analysis::from_single_file(src.to_owned(), Arc::new(cwd));
    let config = DiagnosticsConfig::test_sample();

    match analysis.full_diagnostics(&config, AssistResolveStrategy::None, file_id) {
        Ok(diagnostics) => diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .map(format_diagnostic)
            .collect(),
        Err(cancelled) => vec![format!(
            "rust-analyzer diagnostics cancelled: {cancelled:?}"
        )],
    }
}

fn format_diagnostic(diagnostic: Diagnostic) -> String {
    format!("{}: {}", diagnostic.code.as_str(), diagnostic.message)
}

fn union_keys<V>(left: &BTreeMap<String, V>, right: &BTreeMap<String, V>) -> Vec<String> {
    left.keys()
        .chain(right.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn export_symbol_for(path: &str) -> String {
    let mut symbol = String::from("hot_rust_patch");
    for segment in path.split("::") {
        symbol.push('_');
        symbol.push_str(segment);
    }
    symbol
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Route::NoChange => write!(f, "NoChange"),
            Route::BodyOnly { targets } => {
                write!(f, "BodyOnly")?;
                for target in targets {
                    write!(f, " path={} export={}", target.path, target.export_symbol)?;
                }
                Ok(())
            }
            Route::Structural { reasons } => write!(f, "Structural reasons={reasons:?}"),
            Route::Invalid { errors } => write!(f, "Invalid errors={errors:?}"),
        }
    }
}

enum RouteExpectation {
    NoChange,
    BodyOnly(&'static str),
    Structural(&'static str),
    Invalid,
}

impl RouteExpectation {
    fn assert_matches(&self, route: &Route) {
        match (self, route) {
            (Self::NoChange, Route::NoChange) => {}
            (Self::BodyOnly(path), Route::BodyOnly { targets }) => {
                assert_eq!(targets.len(), 1, "expected exactly one patch target");
                assert_eq!(targets[0].path, *path);
                assert_eq!(targets[0].export_symbol, export_symbol_for(path));
            }
            (Self::Structural(reason), Route::Structural { reasons }) => {
                assert!(
                    reasons.iter().any(|candidate| candidate == reason),
                    "expected structural reason {reason:?}, got {reasons:?}"
                );
            }
            (Self::Invalid, Route::Invalid { errors }) => {
                assert!(
                    !errors.is_empty(),
                    "invalid route should carry syntax errors"
                );
            }
            _ => panic!("expected {}, got {route}", self.label()),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::NoChange => "NoChange",
            Self::BodyOnly(_) => "BodyOnly",
            Self::Structural(_) => "Structural",
            Self::Invalid => "Invalid",
        }
    }
}
