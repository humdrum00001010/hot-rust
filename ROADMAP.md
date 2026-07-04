# Roadmap

Five layers. Each is independently provable. Difficulty rises; the hard parts (M3/M4) are
where subsecond/Live++ spent their real effort.

| milestone | goal | proves | difficulty | status |
|---|---|---|---|---|
| **M1** | patch one function's entry padding with a `jmp` to a new body, in-place, same running image | the *heart* — prologue patching works in Rust using the flag | low | **designed, not built** (`poc/`) |
| **M2** | recompile a crate → dylib, load it, patch old→new across images (far jump / trampoline) | patching to *freshly compiled* code, not just a sibling fn | medium | not started |
| **M3** | change detection: rust-analyzer (`ra_ap_*`) as the oracle — which fn changed + patchable? | the *watcher* half of the pipeline; safety gate | medium–high | specced |
| **M4** | symbol resolution: find the old fn's address in the running process robustly | source-edit ↔ running-binary identity mapping | fiddly | specced |
| **M5** | wire to a real target (e.g. `rhwp`'s native render/layout entry) | end-to-end usefulness on a real codebase | integration | future |

## M1 — the heart (do this first)

Self-contained, no real target yet. In one native binary:

1. Build with `RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16"`.
2. Two functions: `target()` returns 1, `replacement()` returns 2 (both `#[inline(never)]`,
   `black_box` the returns so the optimizer can't const-fold the call).
3. `patch(old, new)`: `VirtualProtect` → write `E9 rel32` over the entry NOPs → restore + flush.
4. `main`: call `target()` → **1**; `patch(target, replacement)`; call `target()` → **2**.

Success = a *direct call* to `target()` returns the new value after the patch, with the call
site untouched. That proves innermost, transparent, in-place patching — the Live++ property.

Verification bonus: dump the first 16 bytes at `target as *const u8` before patching; they
should be NOPs (`0x90` or multi-byte `0f 1f ...`), confirming `-Zpatchable-function-entry` took.

See `poc/src/m1.rs` for the designed code and `poc/NOTES.md` for the build command.

## M2 — patch to freshly compiled code

- Put a reloadable function in a `cdylib`, compiled with the flag.
- Load it (`LoadLibrary`/`libloading`), find the new function's export.
- The new code is likely >±2GB from the old → `E9 rel32` won't reach → emit an **absolute
  jump** (`FF 25` RIP-relative + 8-byte target, ~14 bytes; need `-Zpatchable-function-entry`
  large enough) or a **trampoline** in a page allocated near the old function.
- Patch old→new. Prove: reload a changed dylib, old direct call hits new code.

## M3 — rust-analyzer as the change oracle

- Embed `ra_ap_ide` / `ra_ap_hir` / `ra_ap_load-cargo` (published rust-analyzer-as-a-library).
- On edit: query the salsa DB for the invalidated function(s) and classify:
  `BodyOnly` (patch) / `Structural` (rebuild) / `Invalid` (wait).
- Emit the changed `DefId` + resolved symbol to the driver.
- This replaces a dumb file-watcher with a semantic, safe, minimal-scope trigger.

## M4 — symbol resolution

- Map the edited source item → the symbol in the running image.
- Options: a registration table/macro that records `&fn` addresses at startup; or parse the
  image's symbol table / PDB (Live++'s approach on Windows).
- Handle generics: a source edit to a generic fn maps to *N* monomorphized symbols.

## M5 — real target

- Wire the pipeline to a native debug harness of a real crate (e.g. `rhwp`'s
  `render_page_svg_native` path), inlining off, patchable-entry on.
- Edit a layout function → see the next render reflect it, no rebuild.

## Non-goals (permanent)

- **wasm** — impossible (immutable code). Use subsecond there.
- **state migration / struct-layout changes** — rebuild.
- **release/optimized builds** — inlining destroys patchability; this is a dev-loop tool.
