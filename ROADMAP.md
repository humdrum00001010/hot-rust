# Roadmap

The repository now keeps only the service-shaped implementation. The old standalone experiment
binaries were removed; their useful findings are represented in `hr`, `hr_runtime`, and the
patch backend modules.

## Current Service

- `src/hr.rs` is the `hr cargo ...` entrypoint.
- `src/hr/ra.rs` owns the private rust-analyzer LSP process and server-side watching.
- `src/hr/cargo_driver.rs` owns Cargo command translation and target launch.
- `src/hr/live.rs` owns live source discovery, patch build orchestration, and patch RPCs.
- `src/hr_runtime.rs` is the injected target-side runtime that resolves symbols and patches
  inside the process.
- `src/hr/patch/` owns patch artifact generation and backend selection.

## Active Backend

The measured path for large real methods is:

```bash
HR_PATCH_BACKEND=shadow-fake
HR_SHADOW_PERSISTENT=1
HR_LIVE_SYMBOL=render_node
```

That path keeps a persistent generated fake crate, rewrites the live function and generated
stubs, compiles a unique dylib, and routes generated stubs back to old executable symbols.

## Retained Diagnostics

Some backend modes remain as diagnostic paths:

- `HR_PATCH_BACKEND=object-probe`: emits and reports exact-function object evidence.
- `HR_PATCH_BACKEND=cgu-probe`: reports dirty incremental CGU object evidence.
- `HR_PATCH_BACKEND=shadow-stub`: legacy full shadow-stub baseline.
- `HR_PATCH_BACKEND=shadow-mini`: legacy pruned shadow baseline.

`HR_PATCH_BACKEND=object` is intentionally a dead-end placeholder until exact-function object
relocation is wired. `HR_PATCH_BACKEND=cgu` is still experimental and should be treated as a
runtime object-loader path, not the default service route.

## Next Work

- Replace configured `HR_LIVE_SYMBOL` with full-project body-diff work items.
- Add a stronger patchability gate before runtime patching, including rebuild routing for
  signature/type/layout changes.
- Reduce per-edit shadow-fake work by caching method-stub planning where possible.
- Harden repeated live patches: recovery after bad patch artifacts, rollback policy, and
  target-process crash containment.
- Harden `hr_runtime` object loading before promoting CGU/object paths.
- Keep `rhwp_core` as the worst-case benchmark, especially `SvgRenderer::render_node`.

## Non-goals

- wasm support.
- state migration for struct-layout changes.
- optimized/release builds with inlining.
