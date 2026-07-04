# Roadmap

Five layers. Each is independently provable. Difficulty rises; the hard parts (M3/M4) are
where subsecond/Live++ spent their real effort.

| milestone | goal | proves | difficulty | status |
|---|---|---|---|---|
| **M1** | patch one function's entry padding with a branch to a new body, in-place, same running image | the *heart* — prologue patching works in Rust using the flag | low | **implemented** (`poc/`); verified on `x86_64-apple-darwin` under Rosetta and native `aarch64-apple-darwin`, including default `__TEXT` via Frida-style remap-copy |
| **M2** | recompile a crate → dylib, load it, patch old→new across images (far jump / trampoline) | patching to *freshly compiled* code, not just a sibling fn | medium | **implemented** (`poc/src/m2.rs`); verified on native `aarch64-apple-darwin` |
| **M3** | change detection: rust-analyzer (`ra_ap_*`) as the oracle — which fn changed + patchable? | the *watcher* half of the pipeline; safety gate | medium–high | **first executable slice implemented** (`poc/src/m3.rs`); syntax/shape oracle plus single-file `ra_ap_ide` diagnostics gate |
| **M4** | symbol resolution: find the old fn's address in the running process robustly | source-edit ↔ running-binary identity mapping | fiddly | **implemented** (`poc/src/m4.rs`); registration-table resolver feeds M2 patcher |
| **M5** | wire to a native target harness, then a real target (e.g. `rhwp`'s native render/layout entry) | end-to-end usefulness on target-shaped code | integration | **first executable slice implemented** (`poc/src/m5.rs`); external-crate integration still next |

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

## Non-goals (permanent)

- **wasm** — impossible (immutable code). Use subsecond there.
- **state migration / struct-layout changes** — rebuild.
- **release/optimized builds** — inlining destroys patchability; this is a dev-loop tool.
