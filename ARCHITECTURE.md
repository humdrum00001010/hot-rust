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

**How to tap it:** the "which function body changed + is it patchable" query is **not** in the
LSP wire protocol (that exposes diagnostics/symbols, not body-diffs at DefId granularity).
Embed **rust-analyzer as a library** — the published `ra_ap_ide` / `ra_ap_hir` crates — and
query its salsa DB directly. So the watcher runs rust-analyzer's *analysis engine* inside the
driver, not the editor's LSP socket. Bonus: rust-analyzer is already warm in your editor, so
the incremental model is essentially free during development.

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
2. Write a jump (`E9 rel32` if within ±2GB; else absolute/trampoline) over the entry NOPs.
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

This is enough for M1, but production use still needs thread quiescence and page-level
coordination: remapping replaces a whole page, not just four bytes. The older dev-build
contract is still available too: compile hot-patchable functions into a dedicated `__HOTRST`
segment (`hot-segment-arm64` feature), link that segment as `rwx`, write the ARM64 branch
while `pthread_jit_write_protect_np` has disabled per-thread JIT write protection, flush the
instruction cache, and re-enable JIT write protection.

## Data the parties exchange

- rust-analyzer → driver: `{ changed_fn: DefId, kind: BodyOnly | Structural | Invalid, symbol: mangled_name }`
- driver → rustc: "codegen `changed_fn` (or its crate) → patch artifact"
- rustc → engine: patch artifact (dylib/object) with the new function as a locatable symbol
- engine: resolve `old addr` (from the running image via `symbol`) + `new addr` (from patch) → write jump
