# Architecture

## The core mechanism: prologue-patch (Live++ style)

Every hot-patch approach must solve one problem: **redirect calls to a function to a new
version of that function, at runtime.** There are two ways to do it:

- **Jump table / indirection** (subsecond): compile every call as an indirect call through a
  table slot; swap the slot. Transparent only at instrumented call sites; works on wasm
  (it's the *only* thing that works on wasm). Intrusive.
- **Prologue patch** (Live++, *this engine*): overwrite the first bytes of the old function
  with a `jmp` to the new function. Transparent to *all* callers including direct/innermost
  calls, because you rewrite the callee's own machine code. **Native only** (needs writable
  code memory). This is what we build.

### Why the flag matters

Overwriting a function's prologue is unsafe if you clobber a partial instruction. The fix:
compile with **`-Zpatchable-function-entry=N`**, which emits `N` NOP bytes at each function's
entry (LLVM's `patchable-function-entry` attribute — the same primitive the Linux kernel uses
for ftrace/live-patching). Now the first `N` bytes are known NOPs you can safely overwrite
with a jump. A near `jmp rel32` is 5 bytes; a far absolute jump ~12–14 bytes, so pick `N`
generously (e.g. 16) or use a trampoline for far targets.

Confirmed: this flag ships in stock rustc since **1.81** (unstable). No compiler fork needed.
On a stable toolchain, enable with `RUSTC_BOOTSTRAP=1` + `RUSTFLAGS="-Zpatchable-function-entry=16"`.

## The 3-party pipeline

The engine is not one program — it's an orchestration of three components, each doing only
what it is good at. **Do not collapse them.** (rust-analyzer cannot codegen; rustc is not an
incremental change-oracle; neither patches memory.)

### 1. rust-analyzer — the *watcher* (oracle + router)

Used as an analysis engine, **not** a compiler. It maintains an incremental, semantic model
(salsa) of the code as you type. It provides:

| offer | value | strength |
|---|---|---|
| **Minimal scope** | exact `DefId` of the changed function → codegen *one function*, not the crate | strong |
| **Patchability classification** | body-only → patch; signature/struct-field/type/trait/macro → rebuild; → route correctly | strong — *the key one* |
| **Validity gate** | continuous type-check → only fire on a clean edit; never patch broken/half-typed code | strong |
| **Speculative pre-compile** | sees the change *as you type* → stage codegen early so *save* feels instant | strong |
| **Symbol identity** | resolves the edited item to its path/mangled name → tells the patcher which symbol to locate | medium (clean for non-generic) |

**Why it's the safety gate:** prologue-patching's #1 footgun is patching a change that
corrupts the running process (e.g. a struct grew a field but old state in memory didn't). The
LSP is the only party that can tell a body edit from a structural one and *refuse* to patch.
That refusal is what turns "detour patching" into "usable live coding."

**Blind spot:** rust-analyzer sees the *semantic* call graph and generic instantiations, but
**not codegen inlining decisions** (that's rustc/LLVM). An inlined copy of a patched function
won't update, and the LSP can't see it happened. **Mitigation:** build with inlining off
(debug / `-Cinline-threshold=0`) — which is required anyway for the prologue to exist as a
distinct entry.

**How to tap it:** there are two rust-analyzer surfaces, and they serve different purposes.
The real service starts its own rust-analyzer **LSP** process with
`rust-analyzer.files.watcher = "server"` so rust-analyzer owns project file watching and
workspace reload state. We deliberately do not build our own file watcher and we do not depend
on an editor-owned LSP socket.

The exact "which function body changed + is it patchable" query is still **not** in the LSP
wire protocol (that exposes diagnostics/symbols/status, not body-diffs at DefId granularity).
The service currently uses LSP activity as the watch signal and `HR_LIVE_SYMBOL` as the narrow
target selector. A full semantic body-diff oracle is still future work.

### 2. rustc — codegen

Given "recompile `fn foo`", produce the patch machine code. rust-analyzer cannot do this (no
LLVM, no object files — it's a front-end). Realistic granularity:

- **v0:** recompile the changed *crate/module* to a fresh dylib (simplest; incremental cargo
  helps).
- **v1 (ThinLink-style):** drive rustc to emit just the changed function's object and
  partial-link a patch. Hard; the subsecond/ThinLink hard part. Target ~130ms like subsecond.

Ceiling: codegen of one function needs crate context + monomorphization + LLVM, so realistic
is **on-save sub-second**, not literal per-keystroke.

### 3. the engine — the patcher (this repo)

Given (old function address in the running process, new function address in the loaded patch):

1. `VirtualProtect` the old prologue → writable.
2. Write a jump over the entry NOPs: `FF 25` + u64 on x86-64, `ldr x16; br x16; u64` on
   ARM64, or a platform-specific branch/trampoline when appropriate.
3. Restore protection + `FlushInstructionCache`.
4. Next call to the old function — direct or innermost — lands on the new body.

In-flight functions on the stack are fine: the jump only affects the *next* call (no unwind
needed, unlike the jump-table approach). State already run is not undone (inherent limit).

### Platform caveat: Apple Silicon

The compiler side works on `aarch64-apple-darwin`: `-Zpatchable-function-entry=16` emits four
ARM64 NOP instructions and the patcher can encode a `B imm26` branch. The normal runtime write
path is blocked by macOS code-signing protections in the default executable layout:
`mprotect`, `mach_vm_protect`, and `mach_vm_write` fail against the signed `__TEXT` page.
Apple `ld` also forces `max_prot == init_prot` for non-x86-64 targets, while dyld rejects
`__TEXT` loaded as initial `rwx`.

The working default-`__TEXT` path avoids making the original page writable. It follows the
same broad shape as Frida's Darwin code-patching path: copy the target code page, write the
branch into the copy, mark the copy RX, then `mach_vm_remap` that patched page over the
original mapping with `VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE`. On this machine that changes the
page from code-signed file-backed `__TEXT` to a private RX mapping and direct calls to the
old function enter the replacement.

This works for the service runtime, but production use still needs thread quiescence and
page-level coordination: remapping replaces a whole page, not just four bytes.

## Data the parties exchange

- rust-analyzer → driver: `{ changed_fn: DefId, kind: BodyOnly | Structural | Invalid, symbol: mangled_name }`
- driver → rustc: "codegen `changed_fn` (or its crate) → patch artifact"
- rustc → engine: patch artifact (dylib/object) with the new function as a locatable symbol
- engine: resolve `old addr` (from the running image via `symbol`) + `new addr` (from patch) → write jump

The current service resolves the old address from the built executable's native symbol table
and uses rust-analyzer symbol/source discovery to find the source body. Registration-table
experiments were removed from the codebase.

`hr` adds the dev-loop supervisor boundary:

- User runs `hr cargo <args...>` instead of `cargo <args...>`.
- `hr` starts rust-analyzer LSP first, asks for server-side file watching, and handles the
  minimal LSP requests rust-analyzer needs from a client.
- `hr` owns Cargo invocation and always supplies `RUSTC_BOOTSTRAP=1` plus
  `-Zpatchable-function-entry=16`, preserving any caller-provided `RUSTFLAGS`.
- For `cargo run`, `hr` builds with Cargo JSON output, finds the executable artifact, and
  launches the target process itself. In live mode it injects `libhr_runtime`, waits for the
  target-side `HR_SOCKET`, and sends a patch command after rust-analyzer reports project
  activity or after an explicit rust-analyzer symbol refresh. The first RPC path is still
  configured-symbol based. Free functions use a tiny same-signature patch crate; associated
  methods can use a broad shadow copy of the target crate with a same-module wrapper export.
  `HR_PATCH_BACKEND=shadow-stub` extends that shadow path by rewriting selected helper calls in
  the edited method body to generated exported stubs, then asking the runtime to patch those
  stubs to the old executable's helper functions before it patches the old method entry.
  `HR_PATCH_BACKEND=shadow-mini` keeps that runtime model and additionally prunes unrelated
  shadow-copy function bodies while preserving selected source prefixes for ABI/layout and real
  render behavior. This is still not full shell synthesis, but it proves the first compile-input
  shrink against `SvgRenderer::render_node`.
  `HR_PATCH_BACKEND=shadow-fake` is the next generated-artifact step: it exports generated
  method stubs for direct same-impl calls, prunes function bodies even inside the preserved
  source surface, strips unused serde derive/helper attrs, and builds the unique patch crate
  with incremental disabled. `HR_PATCH_BUILD_ONLY=1` measures that path without launching a
  target process; the current real `rhwp` `render_node` patch crate build is 2.82s.
  `HR_SHADOW_PERSISTENT=1` keeps the generated fake crate stable for rustc incremental reuse
  while copying the built dylib to a unique path for loader safety; after the cold setup, a real
  body-only `render_node` edit measured a 1.68s patch-crate build.
- `HR_PATCH_BACKEND=object-probe` confirms the lower-level codegen direction: for a
  self-contained edited body, rustc emits a relocatable Mach-O object in the hot path without
  rebuilding the target crate. Large private methods still need the original crate/module
  context before object emission; the remaining runtime work is object relocation/fixup rather
  than caller recompilation.
- `HR_PATCH_BACKEND=cgu-probe` confirms the lower-level method direction: for a private
  module-heavy method such as `SvgRenderer::render_node`, `hr` can rerun the real target with
  `cargo rustc ... -- -Z no-link`, find the updated incremental CGU object that defines the
  already-running mangled symbol, and skip final executable linking/restart. This is still a
  timing-only mode. `HR_PATCH_BACKEND=cgu` sends that object to the target runtime, which maps
  the loadable Mach-O sections with MAP_JIT/RWX fallback, resolves Rust-private symbols from the
  running executable's normal Mach-O symbol table, applies the ARM64 relocations used by the
  real `render_node` CGU (`BRANCH26`, `PAGE21`, `PAGEOFF12`, `UNSIGNED`, `ADDEND`), emits
  branch stubs for out-of-range calls, and patches the old entry to the loaded symbol.
  `HR_CODEGEN_UNITS=N` raises rustc's `-Ccodegen-units` for partitioning experiments, but the
  real `rhwp` probe at `N=1024` did not split `SvgRenderer::render_node` into a smaller object.
  The persistent `shadow-fake` path now also uses source-only staging and rewrites the generated
  fake crate root to expose only the modules needed by `render_node`; against real `rhwp`, the
  patch-crate build measured 0.86s unchanged and 0.83s for a real body-only edit after caching
  executable symbols for generated stubs. The persistent fake crate now reuses the already-stubbed
  skeleton and rewrites only the transformed live source file per edit, so whole-tree pruning is
  no longer on the hot path. Excluding target-build/source-discovery setup, the current real
  `render_node` hot patch generation path is about 1.37s; the remaining per-edit bottlenecks are
  patch rustc and method-stub rewriting. Full body-diff oracle support and generated
  ABI-compatible type/layout shells still belong to the next service hardening step.
