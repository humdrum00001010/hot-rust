# Roadmap

Six layers. Each is independently provable. Difficulty rises; the hard parts (M3/M4) are
where subsecond/Live++ spent their real effort.

| milestone | goal | proves | difficulty | status |
|---|---|---|---|---|
| **M1** | patch one function's entry padding with a branch to a new body, in-place, same running image | the *heart* — prologue patching works in Rust using the flag | low | **implemented** (`poc/`); verified on `x86_64-apple-darwin` under Rosetta and native `aarch64-apple-darwin`, including default `__TEXT` via Frida-style remap-copy |
| **M2** | recompile a crate → dylib, load it, patch old→new across images (far jump / trampoline) | patching to *freshly compiled* code, not just a sibling fn | medium | **implemented** (`poc/src/m2.rs`); verified on native `aarch64-apple-darwin` |
| **M3** | change detection: rust-analyzer (`ra_ap_*`) as the oracle — which fn changed + patchable? | the *watcher* half of the pipeline; safety gate | medium–high | **first executable slice implemented** (`poc/src/m3.rs`); syntax/shape oracle plus single-file `ra_ap_ide` diagnostics gate |
| **M4** | symbol resolution: find the old fn's address in the running process robustly | source-edit ↔ running-binary identity mapping | fiddly | **implemented** (`poc/src/m4.rs`); registration-table resolver feeds M2 patcher |
| **M5** | wire to a native target harness, then a real target (e.g. `rhwp`'s native render/layout entry) | end-to-end usefulness on target-shaped code | integration | **first executable slice implemented** (`poc/src/m5.rs`); external-crate integration still next |
| **M6** | `hr cargo ...` supervisor: start hot service before Cargo, run rust-analyzer server watcher, inject flags, launch target | the operational dev-loop boundary | integration | **first service slice implemented** (`poc/src/hr.rs` + `poc/src/hr_runtime.rs`); target-side patch RPC works for configured same-signature function body edits |

## M1 — the heart (do this first)

Self-contained, no real target yet. In one native binary:

1. Build with `RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16"`.
   On macOS/x86-64, also pass `-Clink-arg=-Wl,-segprot,__TEXT,rwx,rx` so the
   `__TEXT` segment starts `r-x` but can temporarily become writable. On native
   Apple Silicon, the default path falls back to a Frida-style patched-page remap because
   direct writes to signed `__TEXT` are blocked. The optional `hot-segment-arm64` feature
   plus `-Clink-arg=-Wl,-segprot,__HOTRST,rwx,rwx` puts `target()` in a dedicated hot-code
   segment and writes it under `pthread_jit_write_protect_np`.
2. Two functions: `target()` returns 1, `replacement()` returns 2 (both `#[inline(never)]`,
   `black_box` the returns so the optimizer can't const-fold the call).
3. `patch(old, new)`: make the old entry writable → write the architecture branch
   (`E9 rel32` on x86-64, `B imm26` on ARM64) over the entry NOPs → restore + flush.
4. `main`: call `target()` → **1**; `patch(target, replacement)`; call `target()` → **2**.

Success = a *direct call* to `target()` returns the new value after the patch, with the call
site untouched. That proves innermost, transparent, in-place patching — the Live++ property.

Verification bonus: dump the first 16 bytes at `target as *const u8` before patching; they
should be NOPs (`0x90` or multi-byte `0f 1f ...`), confirming `-Zpatchable-function-entry` took.

See `poc/src/m1.rs` for the implementation and `poc/NOTES.md` for build commands.

Current platform result:

- `x86_64-apple-darwin` under Rosetta: verified locally. Direct calls to `target()` return
  `2` after the prologue patch.
- Native `aarch64-apple-darwin`: verified locally with `--features hot-segment-arm64`.
  Direct calls to `target()` return `2` after patching the first ARM64 NOP to `B imm26`.
- Native `aarch64-apple-darwin` without the hot segment: verified locally. The direct write
  route still fails (`mprotect`/`mach_vm_protect`/`mach_vm_write`), but the fallback copies
  the whole code page, patches the copy, marks it RX, and remaps it over the original
  `__TEXT` page. Direct calls to `target()` then return `2`.

## M2 — patch to freshly compiled code

- Put a reloadable function in a `cdylib`, compiled with the flag.
- Load it (`LoadLibrary`/`libloading`), find the new function's export.
- The new code is likely >±2GB from the old → `E9 rel32` won't reach → emit an **absolute
  jump** (`FF 25` RIP-relative + 8-byte target, ~14 bytes; need `-Zpatchable-function-entry`
  large enough) or a **trampoline** in a page allocated near the old function.
- Patch old→new. Prove: reload a changed dylib, old direct call hits new code.

Current proof:

- `poc/src/m2.rs` creates a temporary patch crate at runtime, spawns Cargo to build it as a
  `cdylib`, loads `hot_rust_m2_replacement` with `dlopen`/`dlsym`, and patches the old
  `target()` entry to an absolute jump into that dylib.
- On ARM64 the cross-image entry stub is exactly 16 bytes:
  `ldr x16, #8; br x16; <u64 target>`. This avoids the ±128MB limit of `B imm26`.
- Native `aarch64-apple-darwin`: verified locally. Direct writes to the signed `__TEXT` page
  still fail, then the Frida-style page remap fallback succeeds. Direct calls to `target()`
  return `2` from the freshly loaded dylib.

## M3 — rust-analyzer as the change oracle

- Embed `ra_ap_ide` / `ra_ap_hir` / `ra_ap_load-cargo` (published rust-analyzer-as-a-library).
- On edit: query the salsa DB for the invalidated function(s) and classify:
  `BodyOnly` (patch) / `Structural` (rebuild) / `Invalid` (wait).
- Emit the changed `DefId` + resolved symbol to the driver.
- This replaces a dumb file-watcher with a semantic, safe, minimal-scope trigger.

Current proof:

- `poc/src/m3.rs` uses rust-analyzer's `ra_ap_syntax` parser/AST layer to parse old/new source
  snapshots, recover module-qualified item paths, and compare function signatures, function
  bodies, and struct shapes.
- It also runs the edited source through `ra_ap_ide::Analysis::from_single_file(...)` and
  `full_diagnostics(...)`, routing RA error diagnostics to `Invalid` before patching.
- Verified routes:
  - unchanged source -> `NoChange`
  - same function signature with changed body -> `BodyOnly`, emitting a patch export symbol
  - function signature change -> `Structural`
  - struct layout/shape change -> `Structural`
  - syntax errors -> `Invalid`
  - single-file semantic/type errors -> `Invalid`
- This is the M3 oracle kernel. Full project loading/Cargo graph support via `ra_ap_load-cargo`
  remains the next hardening step before calling M3 complete for arbitrary real crates.

## M4 — symbol resolution

- Map the edited source item → the symbol in the running image.
- Options: a registration table/macro that records `&fn` addresses at startup; or parse the
  image's symbol table / PDB (Live++'s approach on Windows).
- Handle generics: a source edit to a generic fn maps to *N* monomorphized symbols.

Current proof:

- `poc/src/m4.rs` uses the registration-table route. The live process records source path,
  expected patch export, signature key, and old function entry address for patchable
  functions.
- The resolver accepts an M3-style intent:
  `{ source_path: "render::paint", patch_export: "hot_rust_patch_render_paint", signature }`.
- It rejects missing paths, stale export symbols, and stale signatures before patching.
- After resolution, it validates the old entry's patch padding, builds a patch `cdylib` with
  the requested export symbol, resolves that export with `dlsym`, and reuses the M2 absolute
  jump patch path.
- Native `aarch64-apple-darwin`: verified locally. `render::paint(10)` changes from `11` to
  `110` through the resolved live address, while `render::stable(10)` remains `17`.

## M5 — real target

- Wire the pipeline to a native debug harness of a real crate (e.g. `rhwp`'s
  `render_page_svg_native` path), inlining off, patchable-entry on.
- Edit a layout function → see the next render reflect it, no rebuild.

Current proof:

- `poc/src/m5.rs` defines a target-shaped native render/layout harness with
  `layout::render_page_svg_native(PageInput) -> LayoutMetrics`.
- It feeds old/new source snapshots for that function through the M3 oracle. The edit is
  classified as `BodyOnly` and emits `hot_rust_patch_layout_render_page_svg_native`.
- It resolves that source path/export/signature through the M4 live registry to the old
  function entry address.
- It builds a patch `cdylib`, loads the requested export, and uses the M2/M4 absolute-jump
  patch path against the resolved address.
- Native `aarch64-apple-darwin`: verified locally. The target frame changes from
  `page=3 content=760x1060 bands=9` to `page=3 content=720x1040 bands=17` on the next direct
  render call, while the stable checksum remains `1923`.

Still open before calling this production-real:

- Load a full Cargo project with `ra_ap_load-cargo` instead of single-file snapshots.
- Replace the hand-written registry with a macro/registration crate usable by an external
  target.
- Wire the same flow to a real application entry such as `rhwp`'s native render/layout path.

## M6 — `hr cargo ...` service boundary

- `hr` is the command users run instead of Cargo: `hr cargo <args...>`.
- It starts before Cargo, prepares a hot-session env (`HR_SESSION_ID`, `HR_SOCKET`,
  `HR_WORKSPACE_ROOT`), appends `-Zpatchable-function-entry=16`, and sets
  `RUSTC_BOOTSTRAP=1`.
- It spawns its own rust-analyzer LSP process and configures `rust-analyzer.files.watcher =
  "server"`. This is deliberately not a custom file watcher and not the editor's socket.
- Non-`run` Cargo commands pass through under that env.
- `cargo run` is translated to `cargo build --message-format=json-render-diagnostics`; `hr`
  parses the executable artifact and launches the target itself so the patch RPC stays attached
  to the process lifetime.
- With `HR_LIVE_SYMBOL=<symbol>`, `hr` injects `libhr_runtime`, waits for the target-side Unix
  socket, discovers the function source without a path argument, and refreshes the source after
  rust-analyzer activity or an explicit rust-analyzer symbol refresh.

Current proof:

- `poc/src/hr.rs` implements the wrapper, a minimal LSP client, rust-analyzer request
  responses, server-status/diagnostics logging, Cargo env injection, build-then-launch for
  `cargo run`, and the live patch driver.
- `poc/src/hr_runtime.rs` is the injected target-side runtime. It binds `HR_SOCKET`, resolves
  the old symbol in the main executable, loads the patch dylib, and patches from inside the
  process with the same copy/remap code path proven in M1/M2.
- rust-analyzer is shut down through a proper LSP shutdown/exit handshake after it reports
  `quiescent=true`; this avoids teardown panics while RA is still reloading the workspace.
- Verified locally:
  - `target/debug/hr cargo --version`
  - `target/debug/hr cargo check --bin m1`
  - `target/debug/hr cargo run --bin m1`
  - `RUSTFLAGS="-Zcrate-attr=feature(if_let_guard)" target/debug/hr cargo run --features m3-oracle --bin m5`
  - real renderer proof: `HR_LIVE_SYMBOL=escape_xml target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 160`, then edit the existing `rhwp_core/src/renderer/svg.rs` `escape_xml` body. The same `rhwp bench` process resolved `renderer::svg::escape_xml`, emitted `HOT_RUST_RENDER_ESCAPE_XML_ONCE` from the patched body, then accepted a second runtime patch after restoring the source body.

Still open:

- Replace the current configured-symbol path with M3/full-project body-diff work items.
- Connect the M3 oracle to full Cargo project state instead of single-file snapshots.
- Generalize codegen support for functions whose bodies depend on crate-local helper types or
  imports; the current body-copy patch builder works for self-contained same-signature
  functions such as `escape_xml(&str) -> String`.
- Promote the experimental `HR_PATCH_BACKEND=object-probe` path into a real installer. The
  probe already emits a Mach-O object for a self-contained edited function in ~0.07s; it now
  needs runtime relocation loading (`BRANCH26`, `PAGE21`, `PAGEOFF12`, data pointers, unwind
  limits) against the running process images.
- Promote the experimental `HR_PATCH_BACKEND=cgu-probe` path into the method installer. For
  associated methods and large module-local bodies, the right artifact is rustc's real dirty
  incremental CGU object, not a standalone function crate. The broad shadow-crate path can
  hot-swap `SvgRenderer::render_node(&mut self, &RenderNode)` by copying the target crate,
  appending a same-module wrapper, compiling a uniquely named patch `cdylib`, and patching the
  live method entry, but it costs around 15s. The dirty-CGU probe found the original
  `render_node` object after `cargo rustc --bin rhwp -- -Z no-link` in 5.29s on edit and 4.73s
  on restore, without source directives or wrappers. `HR_PATCH_BACKEND=cgu` now installs that
  object directly with a target-side Mach-O/ARM64 loader, applying the loadable CGU relocations
  and patching the live method entry. Remaining work is loader hardening: more relocation
  forms, unwind behavior, writable data policy, and edits that introduce symbols absent from
  the old executable.
- Grow the `HR_PATCH_BACKEND=shadow-stub` route into a fake-compiler backend. The first real
  proof rewrites selected `render_node` helper calls to generated exported stubs, patches those
  stubs back to old executable symbols through `libhr_runtime`, and then patches `render_node`.
  `HR_PATCH_BACKEND=shadow-mini` adds a body-pruned shadow copy that kept the real renderer/model
  surface but stripped 4,067 unrelated function bodies across 247 files, cutting the measured
  real `render_node` shadow build to 8.78s. `HR_PATCH_BACKEND=shadow-fake` now prunes all copied
  function bodies except the live body/stubs, strips unused serde derives/attrs, disables
  incremental for the throwaway crate, and measures 2.82s in `HR_PATCH_BUILD_ONLY=1` against the
  same real `rhwp` source. `HR_SHADOW_PERSISTENT=1` keeps that generated crate stable and measured
  1.68s for a real body-only `render_node` edit after the cold setup. The next prune pass copies
  only source/build inputs into staging and rewrites the generated crate root to expose only the
  modules needed by the hot function; the real `render_node` patch-crate build is now 0.86s
  unchanged and 0.83s for an actual body edit after caching executable symbols for generated
  stubs. The persistent fake crate now reuses the already-stubbed skeleton and rewrites only the
  transformed live source file per edit, so whole-tree pruning is no longer on the hot path.
  Excluding target-build/source-discovery setup, the current hot patch generation path is about
  1.37s; remaining per-edit work is dominated by patch rustc and method-stub rewriting. The
  remaining work is to synthesize
  ABI-compatible type/layout shells and update only the transformed live file so the shadow crate
  no longer has to copy broad source prefixes for private method context.
- Harden long-running service behavior across repeated edits and rebuild-routed changes.

## Non-goals (permanent)

- **wasm** — impossible (immutable code). Use subsecond there.
- **state migration / struct-layout changes** — rebuild.
- **release/optimized builds** — inlining destroys patchability; this is a dev-loop tool.
