//! Shadow-crate patch backend.
//!
//! `ShadowFake` is the current large-method path. The older `shadow-stub` and
//! `shadow-mini` modes are retained as comparison baselines and diagnostic
//! fallbacks.

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::super::rust_source::{
    body_indent_for, containing_impl_type, direct_same_impl_callees, dot_method_callees,
    extract_function, function_callees, function_has_type_parameters, function_params_open,
    matching_paren, method_receiver, param_binding_name, patch_signature,
    prune_function_bodies_in_source_except, split_top_level_commas, ParsedFunction,
};
use super::super::symbols::BinarySymbolResolver;
use super::super::util::{
    cargo_command, dylib_filename, file_uri_to_path, log_timing, merged_rustflags,
};
use super::super::{SHADOW_PERSISTENT_ENV, SHADOW_STUBS_ENV};
mod tree;
mod xref;

pub(crate) use xref::ShadowXrefCache;

use super::object::method_object_probe_signature;
use super::{BuiltLivePatch, PatchBackend, PatchStub};
use tree::{
    copy_unique_patch_dylib, copy_workspace_for_fake_shadow, copy_workspace_for_shadow,
    fake_shadow_crate_ready, persistent_shadow_crate, prune_shadow_function_bodies,
    rewrite_shadow_manifest, strip_fake_serde_derives, sync_shadow_tree_changed,
    write_fake_crate_root,
};
use xref::{
    extract_free_function_definition, extract_method_definition, function_definitions_by_name,
    method_definitions_by_name, same_path, FunctionDefinition, MethodDefinition,
};

pub(super) fn build_shadow_crate_patch_dylib(
    workspace_root: &Path,
    source_uri: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
) -> Result<BuiltLivePatch, Box<dyn Error>> {
    // DEPRECATED BLOCK: legacy method fallback. It copies the full workspace
    // and appends only a wrapper, which made it too heavy for the current
    // service path. Keep it only for old measurements and comparison.
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

pub(super) fn build_shadow_stub_patch_dylib(
    workspace_root: &Path,
    executable: &Path,
    source_uri: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
    prune_shadow: bool,
    fake_target_methods: bool,
    cached_symbol_resolver: Option<&BinarySymbolResolver>,
    xref_cache: Option<&mut ShadowXrefCache>,
) -> Result<BuiltLivePatch, Box<dyn Error>> {
    // Shared implementation for the shadow family. `shadow-stub` and
    // `shadow-mini` are legacy modes; `shadow-fake` is the path currently used
    // for real `rhwp` render_node patch builds.
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
    if requested_stubs.is_empty() && !fake_target_methods {
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

    let persistent_fake = fake_target_methods && persistent_shadow_enabled();
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
    // DEPRECATED BLOCK: manual free-function stubs from `HR_SHADOW_STUBS`.
    // `shadow-fake` now discovers method/function callees itself; this remains
    // for the old `shadow-stub`/`shadow-mini` baselines and render_node history.
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
        // DEPRECATED BLOCK: whole-tree body pruning for the `shadow-mini`
        // baseline. The product fake-crate path reuses a prepared skeleton and
        // rewrites only the transformed live source on hot updates.
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

    let mut xref_cache = xref_cache;
    if fake_target_methods {
        let start = Instant::now();
        let external_root = if hot_update_only {
            &build_root
        } else {
            staging_root
                .as_ref()
                .ok_or("external method stubs requested without staging root")?
        };
        let already_stubbed = stubs
            .iter()
            .map(|stub| stub.source_symbol.clone())
            .collect::<HashSet<_>>();
        let external_stubs = rewrite_external_dot_method_callees_as_stubs(
            external_root,
            &shadow_source,
            symbol_resolver,
            &patched_body,
            &already_stubbed,
            xref_cache.as_deref_mut(),
        )?;
        if external_stubs.stub_count > 0 {
            println!(
                "hr: shadow-fake exported {} external method stubs",
                external_stubs.stub_count
            );
            stubs.extend(external_stubs.stubs);
        }
        log_timing("shadow-external-method-stubs", start);
    }
    if fake_target_methods {
        let start = Instant::now();
        let external_root = if hot_update_only {
            &build_root
        } else {
            staging_root
                .as_ref()
                .ok_or("external function stubs requested without staging root")?
        };
        let already_stubbed = stubs
            .iter()
            .map(|stub| stub.source_symbol.clone())
            .collect::<HashSet<_>>();
        let external_stubs = rewrite_external_function_callees_as_stubs(
            external_root,
            symbol_resolver,
            &patched_body,
            &already_stubbed,
            xref_cache.as_deref_mut(),
        )?;
        if external_stubs.stub_count > 0 {
            println!(
                "hr: shadow-fake exported {} external function stubs",
                external_stubs.stub_count
            );
            stubs.extend(external_stubs.stubs);
        }
        log_timing("shadow-external-function-stubs", start);
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

pub(crate) fn prewarm_shadow_xrefs_if_ready(
    workspace_root: &Path,
    old_symbol: &str,
    xref_cache: &mut ShadowXrefCache,
) -> Result<(), Box<dyn Error>> {
    if PatchBackend::from_env() != PatchBackend::ShadowFake || !persistent_shadow_enabled() {
        return Ok(());
    }

    let stable = persistent_shadow_crate(workspace_root, old_symbol)?;
    if !fake_shadow_crate_ready(&stable.root) {
        println!("hr: shadow-fake xref prewarm skipped; persistent skeleton not ready");
        return Ok(());
    }

    let start = Instant::now();
    xref_cache.prewarm(&stable.root)?;
    log_timing("shadow-xref-prewarm", start);
    Ok(())
}

fn persistent_shadow_enabled() -> bool {
    std::env::var(SHADOW_PERSISTENT_ENV)
        .map(|value| !matches!(value.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
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
        let old_method_symbol = symbol_resolver.symbol_for_method(impl_type, &callee)?;
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

fn rewrite_external_dot_method_callees_as_stubs(
    root: &Path,
    target_source: &Path,
    symbol_resolver: &BinarySymbolResolver,
    patched_body: &str,
    already_stubbed: &HashSet<String>,
    xref_cache: Option<&mut ShadowXrefCache>,
) -> Result<ShadowMethodStubRewrite, Box<dyn Error>> {
    let mut callees = dot_method_callees(patched_body);
    callees.retain(|callee| !already_stubbed.contains(callee));
    if callees.is_empty() {
        return Ok(empty_stub_rewrite());
    }

    let owned_index;
    let index = if let Some(cache) = xref_cache {
        cache.method_index(root)?
    } else {
        owned_index = method_definitions_by_name(root)?;
        &owned_index
    };
    let mut by_path: BTreeMap<PathBuf, Vec<MethodDefinition>> = BTreeMap::new();
    for callee in callees {
        let Some(definitions) = index.get(&callee) else {
            continue;
        };
        let definitions = definitions
            .iter()
            .filter(|definition| !same_path(&definition.path, target_source))
            .cloned()
            .collect::<Vec<_>>();
        if definitions.len() != 1 {
            continue;
        }
        let definition = definitions[0].clone();
        by_path
            .entry(definition.path.clone())
            .or_default()
            .push(definition);
    }

    let mut stubs = Vec::new();
    let mut skip_names = Vec::new();
    let mut stub_count = 0usize;
    for (path, definitions) in by_path {
        let text = fs::read_to_string(&path)?;
        let mut plans = Vec::new();
        for definition in definitions {
            let method_name = definition.name.clone();
            let Some(function) =
                extract_method_definition(&text, &method_name, &definition.impl_type)
            else {
                println!(
                    "hr: shadow-fake skipped external method stub {method_name}: current fake definition not found"
                );
                continue;
            };
            let stub_symbol = format!("hot_rust_stub_method_{method_name}");
            let old_method_symbol =
                symbol_resolver.symbol_for_method(&definition.impl_type, &method_name)?;
            let plan = method_stub_plan(
                &definition.impl_type,
                &method_name,
                &stub_symbol,
                &old_method_symbol,
                &function,
                &text,
            )?;
            plans.push(plan);
        }
        let rewrite = apply_external_stub_plans(&path, text, plans)?;
        skip_names.extend(rewrite.skip_names);
        stubs.extend(rewrite.stubs);
        stub_count += rewrite.stub_count;
    }

    skip_names.sort();
    skip_names.dedup();
    Ok(ShadowMethodStubRewrite {
        stubs,
        stub_sources: String::new(),
        skip_names,
        stub_count,
    })
}

fn rewrite_external_function_callees_as_stubs(
    root: &Path,
    symbol_resolver: &BinarySymbolResolver,
    patched_body: &str,
    already_stubbed: &HashSet<String>,
    xref_cache: Option<&mut ShadowXrefCache>,
) -> Result<ShadowMethodStubRewrite, Box<dyn Error>> {
    let mut callees = function_callees(patched_body);
    callees.retain(|callee| {
        !already_stubbed.contains(callee) && !callee.starts_with("hot_rust_stub_")
    });
    if callees.is_empty() {
        return Ok(empty_stub_rewrite());
    }

    let owned_index;
    let index = if let Some(cache) = xref_cache {
        cache.function_index(root)?
    } else {
        owned_index = function_definitions_by_name(root)?;
        &owned_index
    };
    let mut by_path: BTreeMap<PathBuf, Vec<FunctionDefinition>> = BTreeMap::new();
    for callee in callees {
        let Some(definitions) = index.get(&callee) else {
            continue;
        };
        if definitions.len() != 1 {
            continue;
        }
        let definition = definitions[0].clone();
        by_path
            .entry(definition.path.clone())
            .or_default()
            .push(definition);
    }

    let mut stubs = Vec::new();
    let mut skip_names = Vec::new();
    let mut stub_count = 0usize;
    for (path, definitions) in by_path {
        let text = fs::read_to_string(&path)?;
        let mut plans = Vec::new();
        for definition in definitions {
            let function_name = definition.name.clone();
            let Some(function) = extract_free_function_definition(&text, &function_name) else {
                println!(
                    "hr: shadow-fake skipped external function stub {function_name}: current fake definition not found"
                );
                continue;
            };
            if function_has_type_parameters(&function.signature, &function_name) {
                continue;
            }
            let stub_symbol = format!("hot_rust_stub_fn_{function_name}");
            let old_function_symbol = match symbol_resolver
                .symbol_for_function(&definition.module_components, &function_name)
            {
                Ok(symbol) => symbol,
                Err(error) => {
                    println!(
                        "hr: shadow-fake skipped external function stub {function_name}: {error}"
                    );
                    continue;
                }
            };
            let plan = match method_stub_plan(
                "",
                &function_name,
                &stub_symbol,
                &old_function_symbol,
                &function,
                &text,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    println!(
                        "hr: shadow-fake skipped external function stub {function_name}: {error}"
                    );
                    continue;
                }
            };
            plans.push(plan);
        }
        let rewrite = apply_external_stub_plans(&path, text, plans)?;
        skip_names.extend(rewrite.skip_names);
        stubs.extend(rewrite.stubs);
        stub_count += rewrite.stub_count;
    }

    skip_names.sort();
    skip_names.dedup();
    Ok(ShadowMethodStubRewrite {
        stubs,
        stub_sources: String::new(),
        skip_names,
        stub_count,
    })
}

fn empty_stub_rewrite() -> ShadowMethodStubRewrite {
    ShadowMethodStubRewrite {
        stubs: Vec::new(),
        stub_sources: String::new(),
        skip_names: Vec::new(),
        stub_count: 0,
    }
}

fn apply_external_stub_plans(
    path: &Path,
    mut text: String,
    mut plans: Vec<MethodStubPlan>,
) -> Result<ShadowMethodStubRewrite, Box<dyn Error>> {
    let original_text = text.clone();
    plans.sort_by_key(|plan| plan.body_start);
    plans.dedup_by_key(|plan| plan.patch_stub.source_symbol.clone());
    for plan in plans.iter().rev() {
        text.replace_range(plan.body_start..plan.body_end, &plan.body_replacement);
    }

    let mut stubs = Vec::new();
    let mut skip_names = Vec::new();
    let mut stub_count = 0usize;
    for plan in plans {
        let stub_needle = format!("fn {}", plan.patch_stub.stub_symbol);
        if !text.contains(&stub_needle) {
            text.push_str(&plan.stub_source);
        }
        skip_names.extend(plan.skip_names.iter().cloned());
        stubs.push(plan.patch_stub);
        stub_count += 1;
    }
    write_if_changed(path, &original_text, &text)?;

    skip_names.sort();
    skip_names.dedup();
    Ok(ShadowMethodStubRewrite {
        stubs,
        stub_sources: String::new(),
        skip_names,
        stub_count,
    })
}

fn write_if_changed(path: &Path, before: &str, after: &str) -> io::Result<bool> {
    if before == after {
        return Ok(false);
    }
    fs::write(path, after)?;
    Ok(true)
}

fn method_stub_plan(
    impl_type: &str,
    method_name: &str,
    stub_symbol: &str,
    old_method_symbol: &str,
    function: &ParsedFunction,
    source: &str,
) -> Result<MethodStubPlan, Box<dyn Error>> {
    let open = function_params_open(&function.signature)
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

fn shadow_stub_symbols(old_symbol: &str) -> Vec<String> {
    // DEPRECATED BLOCK: old manual helper list. It is still accepted as a debug
    // override, but the product path should rely on generated xref stubs.
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
