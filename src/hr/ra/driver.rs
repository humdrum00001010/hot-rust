//! RA-first service driver.
//!
//! This is the top-level runtime shape for `hr cargo ...`: boot rust-analyzer
//! and its project watcher first, then let Cargo/target execution run under that
//! already-live model. Live edits flow back through this driver:
//! code change -> rust-analyzer watcher/model -> LSP activity -> patch work.

use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{maybe_hold_for_project_watch_proof, RustAnalyzerSession};
use crate::cargo_driver::{run_cargo, CargoDriverResult, LiveTargetRun};
use crate::live::{
    build_live_patch_once, discover_live_source_uri, send_object_patch_command, send_patch_command,
    source_snippet, source_text_from_ra_or_disk,
};
use crate::patch::{
    build_function_patch_dylib, build_incremental_cgu_probe, prewarm_shadow_xrefs_if_ready,
    PatchBackend, ShadowXrefCache,
};
use crate::rust_source::extract_function;
use crate::session::{wait_for_socket, HotSession};
use crate::symbols::{source_path_hint_from_symbol, BinarySymbolResolver};
use crate::util::log_timing;

pub(crate) struct RustAnalyzerDriver {
    workspace_root: PathBuf,
    ra: RustAnalyzerSession,
    session: HotSession,
}

impl RustAnalyzerDriver {
    pub(crate) fn boot(workspace_root: PathBuf) -> Result<Self, Box<dyn Error>> {
        println!(
            "hr: ra-driver root {}; rust-analyzer owns project watching",
            workspace_root.display()
        );

        // Layer 1: rust-analyzer owns project observation. It must be running
        // before Cargo so later source changes are seen through RA state.
        let start = Instant::now();
        let ra = RustAnalyzerSession::start(&workspace_root)?;
        log_timing("rust-analyzer-start", start);

        // Layer 2: the hot session is the process/runtime boundary. Cargo and
        // the target inherit this environment, but they do not drive the loop.
        let start = Instant::now();
        let session = HotSession::new(&workspace_root)?;
        log_timing("hot-session", start);
        println!(
            "hr: session {} root {}",
            session.id,
            workspace_root.display()
        );

        Ok(Self {
            workspace_root,
            ra,
            session,
        })
    }

    pub(crate) fn run_cargo(&self, cargo_args: &[String]) -> Result<(), Box<dyn Error>> {
        // Layer 3: Cargo is a backend. It either finishes normally, returns a
        // build-only patch request, or hands back a launched target to drive.
        let start = Instant::now();
        let result = run_cargo(&self.workspace_root, &self.session, cargo_args);
        log_timing("cargo-control", start);
        let result = match result? {
            CargoDriverResult::Completed => Ok(()),
            CargoDriverResult::LiveBuildOnly(request) => build_live_patch_once(
                &self.workspace_root,
                &self.ra,
                &request.executable,
                request.live,
                &request.cargo_side,
                request.bin_name.as_deref(),
            ),
            CargoDriverResult::LiveTarget(target) => self.drive_live_target(target),
        };
        if result.is_ok() {
            maybe_hold_for_project_watch_proof(&self.ra)?;
        }
        result
    }

    fn drive_live_target(&self, mut target: LiveTargetRun) -> Result<(), Box<dyn Error>> {
        // Layer 4: live mode is RA-led. LSP activity selects when to refresh
        // source, patch backends compile artifacts, and runtime RPC installs.
        println!(
            "hr: live mode symbol={} runtime={}",
            target.live.symbol,
            target.live.runtime_dylib.display()
        );
        wait_for_socket(&self.session.socket, Duration::from_secs(10))?;

        let start = Instant::now();
        let symbol_resolver = BinarySymbolResolver::load(&self.workspace_root, &target.executable)?;
        log_timing("live-symbol-index", start);
        let mut xref_cache = ShadowXrefCache::default();
        let start = Instant::now();
        let old_runtime_symbol = symbol_resolver.symbol_for(&target.live.symbol)?;
        log_timing("live-symbol-resolve", start);
        println!(
            "hr: live runtime symbol {} -> {}",
            target.live.symbol, old_runtime_symbol
        );
        let module_hint = source_path_hint_from_symbol(&old_runtime_symbol, &target.live.symbol);
        if let Some(module_hint) = &module_hint {
            println!("hr: live source module hint {}", module_hint.join("::"));
        }
        let source_uri = discover_live_source_uri(
            &self.workspace_root,
            &self.ra,
            &target.live.symbol,
            target.bin_name.as_deref(),
            module_hint.as_deref(),
        )?;
        println!(
            "hr: live source symbol {} uri {}",
            target.live.symbol, source_uri
        );

        let mut source_text = source_text_from_ra_or_disk(&self.ra, &source_uri)?;
        println!("hr: live source text bytes={}", source_text.len());
        let mut current_function =
            extract_function(&source_text, &target.live.symbol).ok_or_else(|| {
                format!(
                    "could not parse function {}; snippet={}",
                    target.live.symbol,
                    source_snippet(&source_text, &target.live.symbol)
                )
            })?;
        let required_signature = current_function.signature.clone();
        println!(
            "hr: live initial {} signature `{}` body-bytes={}",
            target.live.symbol,
            required_signature.trim(),
            current_function.body.len()
        );
        prewarm_shadow_xrefs_if_ready(&self.workspace_root, &target.live.symbol, &mut xref_cache)?;

        let mut patches = Vec::new();
        let mut activity_baseline = self.ra.activity_seq();
        loop {
            if let Some(status) = target.child.try_wait()? {
                if !status.success() {
                    return Err(format!("target exited with {status}").into());
                }
                return Ok(());
            }

            let reason = if let Some(reason) = self
                .ra
                .wait_for_activity_after(activity_baseline, Duration::from_millis(500))?
            {
                activity_baseline = self.ra.activity_seq();
                reason
            } else {
                let _ = self.ra.workspace_symbol_contains(&target.live.symbol)?;
                "workspace/symbol refresh".to_string()
            };

            let next_text = source_text_from_ra_or_disk(&self.ra, &source_uri)?;
            if next_text == source_text {
                continue;
            }
            println!("hr: live source check after rust-analyzer {reason}");
            source_text = next_text;

            let Some(next_function) = extract_function(&source_text, &target.live.symbol) else {
                println!(
                    "hr: live edit seen but {} is not parseable yet; snippet={}",
                    target.live.symbol,
                    source_snippet(&source_text, &target.live.symbol)
                );
                continue;
            };
            if next_function.signature != required_signature {
                println!(
                    "hr: live edit seen but {} signature changed; rebuild required. old=`{}` new=`{}`",
                    target.live.symbol,
                    required_signature.trim(),
                    next_function.signature.trim()
                );
                continue;
            }
            if next_function.body == current_function.body {
                continue;
            }

            // Body-only is the current patchable lane. Signature or layout
            // changes are left to the rebuild route above.
            println!(
                "hr: live source edit {} body bytes {} -> {}",
                target.live.symbol,
                current_function.body.len(),
                next_function.body.len()
            );
            if PatchBackend::from_env() == PatchBackend::CguOnly {
                let probe = build_incremental_cgu_probe(
                    &self.workspace_root,
                    &target.cargo_side,
                    &old_runtime_symbol,
                )?;
                probe.report();
                let object = probe
                    .after
                    .as_ref()
                    .ok_or("dirty-CGU object patch requested but no updated object was found")?;
                send_object_patch_command(&self.session, &old_runtime_symbol, &object.path)?;
                current_function = next_function;
                continue;
            }
            let patch = build_function_patch_dylib(
                &self.workspace_root,
                &target.executable,
                &target.cargo_side,
                &source_uri,
                &old_runtime_symbol,
                &target.live.symbol,
                &target.live.patch_symbol,
                &next_function,
                Some(&symbol_resolver),
                Some(&mut xref_cache),
            )?;
            send_patch_command(&self.session, &old_runtime_symbol, &target.live, &patch)?;
            current_function = next_function;
            patches.push(patch);
        }
    }
}
