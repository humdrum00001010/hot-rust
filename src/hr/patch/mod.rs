mod object;
mod shadow;

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) use object::build_incremental_cgu_probe;
pub(crate) use shadow::{prewarm_shadow_xrefs_if_ready, ShadowXrefCache};

use object::build_function_patch_object_probe;
use shadow::{build_shadow_crate_patch_dylib, build_shadow_stub_patch_dylib};

use super::rust_source::{method_receiver, patch_signature, ParsedFunction};
use super::symbols::BinarySymbolResolver;
use super::util::{cargo_command, dylib_filename, env_flag, merged_rustflags};
use super::{KEEP_PATCH_ROOT_ENV, PATCH_BACKEND_ENV, SHADOW_PRUNE_ENV};

pub(crate) struct BuiltLivePatch {
    root: PathBuf,
    pub(crate) dylib: PathBuf,
    pub(crate) stubs: Vec<PatchStub>,
    cleanup_root: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PatchStub {
    pub(crate) source_symbol: String,
    pub(crate) stub_symbol: String,
    pub(crate) old_symbol: String,
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
pub(crate) enum PatchBackend {
    // Legacy/free-function fallback. Method-shaped edits should use the shadow
    // family below; this standalone dylib path cannot compile most real methods.
    Dylib,
    // Legacy method experiment: full shadow crate plus explicit free-function
    // stubs. Retained for comparison against the fake-crate path.
    ShadowStub,
    // Legacy method experiment: shadow-stub plus broad function-body pruning.
    // Useful as a performance baseline, but not the current rhwp path.
    ShadowMini,
    // Current measured path for large real methods: persistent fake crate,
    // pruned target source, external xref stubs, unique dylib output.
    ShadowFake,
    // Diagnostic only: ask rustc for a relocatable exact-function object and
    // report symbols/relocations. It does not install a runtime patch.
    ObjectProbe,
    // Dead-end installer placeholder. It intentionally errors after the object
    // probe because exact-function object relocation is not wired.
    ObjectOnly,
    // Diagnostic only: inspect Cargo incremental dirty-CGU object emission.
    CguProbe,
    // Experimental live-only object path. The RA driver can send the dirty CGU
    // object to the runtime, but build-only patch generation still rejects it.
    CguOnly,
}

impl PatchBackend {
    pub(crate) fn from_env() -> Self {
        match std::env::var(PATCH_BACKEND_ENV) {
            Ok(value) if value == "dylib" || value == "free-dylib" => Self::Dylib,
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
                    "hr: ignoring unsupported {PATCH_BACKEND_ENV}={value}; using shadow-fake backend"
                );
                Self::ShadowFake
            }
            _ => Self::ShadowFake,
        }
    }

    fn wants_object_probe(self) -> bool {
        matches!(self, Self::ObjectProbe | Self::ObjectOnly)
    }

    fn wants_cgu_probe(self) -> bool {
        matches!(self, Self::CguProbe | Self::CguOnly)
    }
}

pub(crate) fn build_function_patch_dylib(
    workspace_root: &Path,
    executable: &Path,
    cargo_side: &[String],
    source_uri: &str,
    runtime_symbol: &str,
    old_symbol: &str,
    patch_symbol: &str,
    function: &ParsedFunction,
    symbol_resolver: Option<&BinarySymbolResolver>,
    xref_cache: Option<&mut ShadowXrefCache>,
) -> Result<BuiltLivePatch, Box<dyn Error>> {
    let backend = PatchBackend::from_env();
    if backend.wants_cgu_probe() {
        // Diagnostic branch retained from the CGU/object-loader experiments.
        // It gives evidence about rustc output but is not the normal dylib path.
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
        // Diagnostic branch retained from the exact-function object experiment.
        // The installer side never graduated, so `ObjectOnly` deliberately stops.
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
                xref_cache,
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

    // Legacy fallback for free functions. It is still useful for tiny tests, but
    // real method patches go through the shadow backend above.
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
