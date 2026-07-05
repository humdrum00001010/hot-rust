# hot-rust

A native, function-level **hot-patch engine for Rust** — change a function's body in a
running program and have it take effect *without a restart or a full rebuild*. It's Live++'s
mechanism, rebuilt in Rust, on top of compiler support that already ships.

## Why this exists

Rust's edit→run loop is slow (motivating case: the `rhwp` HWP viewer, ~20-min release
builds). We surveyed the field and nothing fits the exact need — *transparent, innermost,
native, function-level* hot-patch for Rust:

| tool | verdict for us |
|---|---|
| **subsecond** (Dioxus) | works & maintained, but **experimental**, needs intrusive `subsecond::call` sites, uses a *jump-table* technique. The only **wasm**-capable option. |
| **Live++** | the gold standard — but **native/Windows-only and does not support Rust** (roadmap "under investigation" since 2022). |
| **hot-lib-reloader** | mature-ish but **stale** (last release 2025-08); native dylib-swap, not transparent, has gotchas (`tracing`, TypeId). |
| **fork rustc** | the compiler knob we'd add (`-Zpatchable-function-entry`) **already ships since 1.81** → forking is pointless. |

So we build the missing piece: the **engine** that consumes the existing compiler flag.

## The architecture in one paragraph

Three parties, each doing only what it's good at:

1. **rust-analyzer — as a *watcher*, not a compiler.** The change oracle: it says *which
   function's body* changed, *whether it's safe to patch* (body-only) vs needs a rebuild
   (signature / type / struct-layout / macro), and only fires when the edit type-checks.
2. **rustc — codegen.** Compiles just the changed function into patch machine code.
3. **the engine (this repo) — the patcher.** Overwrites the old function's entry padding
   (emitted by `-Zpatchable-function-entry`) with a jump to the new code, in the running
   process. Live++-style prologue trampoline.

```
edit → rust-analyzer (what changed? patchable?) → rustc codegen(just that fn) → engine writes the jump
         └─ oracle + router ─┘                     └─ the actual patch bytes ─┘   └─ M1..M5 ─┘
```

## Status (at this capture)

- Architecture + landscape: **understood** (this directory).
- `-Zpatchable-function-entry`: **confirmed** in stock rustc since **1.81** (unstable; the
  general LLVM/GCC `-fpatchable-function-entry` primitive). The Windows-only `-Zhotpatch`
  sugar is separate and was *not* in the checkout we inspected.
- **M1** (prove the in-place prologue jump): **implemented** in `poc/`.
  Verified locally on `x86_64-apple-darwin` under Rosetta and native
  `aarch64-apple-darwin`. On Apple Silicon, direct writes to the signed `__TEXT` page are
  blocked, but a Frida-style copy/remap fallback patches the default `__TEXT` page by mapping
  a patched RX copy over the original page. The `hot-segment-arm64` dev-build feature remains
  as a simpler dedicated hot-code segment path.
- **M2** (patch to freshly compiled dylib code): **implemented** in `poc/`.
  Verified locally on native `aarch64-apple-darwin`: the harness spawns Cargo, builds a
  temporary `cdylib`, loads the exported replacement with `dlopen`/`dlsym`, and patches the
  old function to an absolute ARM64 stub targeting that dylib.
- **M3** (change oracle): **first executable slice implemented** in `poc/`.
  The `m3` harness uses rust-analyzer crates to compare old/new source, route body-only
  function edits to patch, route signature/struct changes to rebuild, and route syntax or
  single-file semantic errors to wait.
- **M4** (live symbol resolution): **implemented** in `poc/`.
  The `m4` harness resolves an M3-style `{ source_path, patch_export, signature }` intent to
  the old function's live entry address through a registration table, validates the patchable
  entry, loads the matching patch dylib export, and patches the resolved address.
- **M5** (end-to-end native target harness): **first executable slice implemented** in `poc/`.
  The `m5` harness composes M3 -> M4 -> M2 against a native render/layout entry and proves the
  next direct frame render observes the edited body without restarting the target process.
- **M6** (`hr cargo ...` supervisor): **first service slice implemented** in `poc/`.
  `hr` starts before Cargo, launches a private rust-analyzer LSP with server-side file
  watching, injects the patchable-entry compiler env, translates `cargo run` into build +
  target launch, and keeps rust-analyzer alive while the target process runs. The current live
  mode injects `libhr_runtime`, builds either a tiny patch dylib or a broad shadow-crate dylib,
  and patches the running target. This was proved against `rhwp`'s real SVG renderer on
  `samples/aift.hwp`, including the large `SvgRenderer::render_node` method.
  `HR_PATCH_BACKEND=object-probe` now also asks rustc to emit a relocatable object for the edited
  function before falling back to the dylib path; this emits quickly for self-contained functions
  and exposes the crate-private context boundary for large methods. `HR_PATCH_BACKEND=cgu-probe`
  reruns the real crate target with `cargo rustc ... -- -Z no-link`, finds the dirty incremental
  CGU object that defines the already-running symbol, and times that path before falling back to
  the dylib installer. `HR_PATCH_BACKEND=cgu` sends that object directly to `libhr_runtime`,
  which now has a first Mach-O/ARM64 object loader for the real dirty CGU path.
  `HR_CODEGEN_UNITS=N` can raise rustc's CGU count for experiments; a real `rhwp` run with
  `N=1024` raised the main crate object count but left `render_node` in the same 533,984-byte
  object, so CGU count alone is not enough to shrink that hot swap unit.
  `HR_PATCH_BACKEND=shadow-stub` proves the other route: the shadow `cdylib` rewrites selected
  helper calls to generated exported stubs, `libhr_runtime` patches those stubs back to helper
  symbols in the old executable, and then patches the old method entry to the new dylib code.
  `HR_PATCH_BACKEND=shadow-mini` adds a first pruning pass: it keeps the renderer/model surface
  needed for real execution, strips unrelated shadow-copy function bodies, and cut the real
  `render_node` shadow build from about 15s to 8.78s in the current proof.
  `HR_PATCH_BACKEND=shadow-fake` shifts the same idea toward a compiler artifact: it rewrites
  direct same-impl calls from `render_node` to generated stubs, prunes all non-live function
  bodies in the copied crate, strips unused serde derives/attrs, and builds the throwaway patch
  crate with incremental disabled. In build-only mode (`HR_PATCH_BUILD_ONLY=1`) against the real
  `rhwp` executable/source, the patch crate build dropped from the 8.57s `shadow-mini` baseline
  to 2.82s without launching the renderer.
  With `HR_SHADOW_PERSISTENT=1`, the generated fake crate keeps a stable path/package name for
  rustc reuse and copies the resulting dylib to a unique file for future `dlopen`; after the first
  setup, a real body-only `render_node` edit measured a 1.68s patch-crate build. The latest fake
  crate pass copies only source/build inputs needed by the shell and rewrites the generated
  `src/lib.rs` to expose only the modules needed by `render_node`; against real `rhwp`, that
  measured 0.86s unchanged and 0.83s after a real body-only `render_node` edit. The next measured
  bottleneck was outside rustc: repeated executable symbol scans for generated stubs, now reduced
  by a per-session symbol index. The persistent fake crate now reuses the already-stubbed
  skeleton and rewrites only the transformed live source file per edit; excluding
  target-build/source-discovery setup, the current real `render_node` hot patch generation path is
  about 1.37s, dominated by patch rustc and method-stub rewriting.

## Hard boundaries (do not forget)

- **Native only.** wasm code is immutable — you cannot patch a prologue in a browser. This
  engine will never work on wasm; that's subsecond's jump-table territory.
- **Body-only.** Signature / struct-layout / type changes cannot be patched — the old state
  in memory would be misinterpreted. These require a rebuild. (rust-analyzer is what detects
  and routes them — that's the safety gate.)
- **No state migration.** Patching code does not undo already-run code or migrate globals.
- **No inlining.** Build with inlining off; an inlined function has no distinct entry to patch.

## Read next

- `ARCHITECTURE.md` — how the engine + the 3-party pipeline work, in detail.
- `ROADMAP.md` — M1..M6 milestones and what each proves.
- `RESEARCH.md` — the full landscape, the decisions and their rationale, and sources.
- `poc/` — the M1 implementation and platform notes.
