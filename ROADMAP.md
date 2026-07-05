# Roadmap

The repository now keeps only the service-shaped implementation. The old standalone experiment
binaries were removed; their useful findings are represented in `hr`, `hr_runtime`, and the
patch backend modules.

## Current Service

- `src/hr.rs` is the `hr cargo ...` entrypoint.
- `src/hr/ra/driver.rs` is the RA-centered service driver: rust-analyzer boots before Cargo or
  target execution, then Cargo work and project-wide live patch decisions are submitted under
  that driver.
- `src/hr/ra/lsp.rs` owns the private rust-analyzer LSP process and server-side watching.
- `src/hr/ra/project.rs` owns the current project function snapshot/diff layer.
- `src/hr/cargo_driver.rs` owns Cargo command translation and target launch.
- `src/hr/live.rs` owns live source discovery, patch build orchestration, and patch RPCs.
- `src/hr_runtime.rs` is the injected target-side runtime that resolves symbols and patches
  inside the process.
- `src/hr/patch/` owns patch artifact generation and backend selection.

## Active Backend

The measured path for large real methods is now the default: persistent `shadow-fake`.
It keeps a generated fake crate, rewrites the live function and generated stubs, compiles a
unique dylib, and routes generated stubs back to old executable symbols. `HR_PATCH_BACKEND`
and `HR_SHADOW_PERSISTENT` are retained as diagnostic overrides.

## Retained Diagnostics

Some backend modes remain as diagnostic paths:

- `HR_PATCH_BACKEND=object-probe`: emits and reports exact-function object evidence.
- `HR_PATCH_BACKEND=cgu-probe`: reports dirty incremental CGU object evidence.
- `HR_PATCH_BACKEND=shadow-stub`: legacy full shadow-stub baseline.
- `HR_PATCH_BACKEND=shadow-mini`: legacy pruned shadow baseline.
- `HR_LIVE_SYMBOL=<fn>`: force the old single-symbol debug route.

`HR_PATCH_BACKEND=object` is intentionally a dead-end placeholder until exact-function object
relocation is wired. `HR_PATCH_BACKEND=cgu` is still experimental and should be treated as a
runtime object-loader path, not the default service route.

## Next Work

- Replace the first project body-diff scanner with a stronger RA semantic DefId oracle.
- Add a stronger patchability gate before runtime patching, including richer rebuild routing for
  type/layout/macro changes.
- Reduce per-edit shadow-fake work by caching method-stub planning where possible.
- Harden repeated live patches: recovery after bad patch artifacts, rollback policy, and
  target-process crash containment.
- Harden `hr_runtime` object loading before promoting CGU/object paths.
- Keep `rhwp_core` as the worst-case benchmark, especially `SvgRenderer::render_node`.

## Non-goals

- wasm support.
- state migration for struct-layout changes.
- optimized/release builds with inlining.
