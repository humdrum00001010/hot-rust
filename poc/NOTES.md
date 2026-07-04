# poc — build & run

## M1 (in-place prologue jump)

Needs the unstable `-Zpatchable-function-entry` flag, unlocked on a stable toolchain via
`RUSTC_BOOTSTRAP=1`.

### Windows / x86-64

```powershell
$env:RUSTC_BOOTSTRAP = "1"
$env:RUSTFLAGS = "-Zpatchable-function-entry=16"
cargo run --bin m1
```

or bash:

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1
```

### macOS / x86-64

On Darwin x86-64 the patcher also needs a linker segment-protection flag: `__TEXT` must load
as read+execute, but its maximum protection must include write so `mprotect` can temporarily
flip the function entry page.

```bash
rustup target add x86_64-apple-darwin
RUSTC_BOOTSTRAP=1 \
RUSTFLAGS="-Zpatchable-function-entry=16 -Clink-arg=-Wl,-segprot,__TEXT,rwx,rx" \
cargo run --target x86_64-apple-darwin --bin m1
```

This has been verified locally on Apple Silicon through Rosetta.

### macOS / native ARM64

Normal signed `__TEXT` pages are not writable on Apple Silicon, even from the current process:
`mprotect`, `mach_vm_protect`, and `mach_vm_write` fail. The default `m1` path now falls back
to a Frida-style whole-page copy/remap: copy the code page, patch the copy, mark it RX, and
`mach_vm_remap` it over the original `__TEXT` page.

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1
```

This has been verified locally on native Apple Silicon. The output includes the direct-write
failure and then:

```
code patch: direct write failed (...); frida-style remap copy succeeded
after  patch: target() = 2
```

The `hot-segment-arm64` feature is still available as a dedicated hot-code-segment route. It
places `target()` into `__HOTRST` and writes the branch with `pthread_jit_write_protect_np`
temporarily allowing writes on this thread.

```bash
RUSTC_BOOTSTRAP=1 \
RUSTFLAGS="-Zpatchable-function-entry=16 -Clink-arg=-Wl,-segprot,__HOTRST,rwx,rwx" \
cargo run --features hot-segment-arm64 --bin m1
```

This has been verified locally on native Apple Silicon.

### macOS / Frida-style remap probe

`m1_frida_style` is the diagnostic version of the native ARM64 fallback. It logs page-info,
the writable-alias attempt, and the copy-remap attempt.

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1_frida_style
```

On this machine:

```
target_page_info: ... flags=CS_VALIDATED
alias_remap: mach_vm_remap ... cur=0x5(rx) max=0x5(rx)
alias_remap: protect_alias_rw kr=2
copy_remap: remap_over_target kr=0 ...
target_after_copy_remap_page_info: ... flags=none
after=2 replacement=2
```

The Frida ARM64 `brk #1337` page-plan probe is intentionally opt-in because it hung here until
the child process was killed:

```bash
HOT_RUST_TRY_PAGE_PLAN=1 \
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1_frida_style
```

### macOS / debugger-parent test

`m1_parent` tests whether an external parent/debugger process can patch a child process's
default `__TEXT` page. It attaches with `ptrace(PT_ATTACHEXC)`, gets the child task port via
`task_for_pid`, and then attempts `mach_vm_protect`/`mach_vm_write`.

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1_parent
```

Run it with enough privilege for `task_for_pid` if needed. On this machine, the attach and task
port succeed, but the write still fails:

```
parent: ptrace attach ok
parent: task_for_pid ok
parent: mach_vm_protect current=2 max=2 retry=2
child: entry_after=[1f, 20, 03, d5, ...]
child: after=1
error: debug remote patch failed: mach_vm_write returned 1
```

### Expected output (roughly)
```
target() entry bytes (expect NOP padding): [0f, 1f, ...]   # or 90 90 ... — proves the flag
before patch: target() = 1
target() entry bytes after patch: [e9, ...]                # x86-64 near jump
# or on ARM64: [.., .., .., 17, 1f, 20, 03, d5, ...]       # B imm26 then remaining NOPs
after  patch: target() = 2   (replacement() itself = 2)
OK: direct call to target() now runs replacement()'s body — call site untouched.
```

If `after patch: target() = 2`, the prologue patch worked: a **direct call** was redirected by
rewriting the callee's own entry. That's the engine's heart.

### Notes / gotchas to watch when first building
- Must be a **debug** (unoptimized) build — `#[inline(never)]` + `opt-level=0` keep `target`
  a real, separate, callable function.
- `black_box` stops the optimizer from const-folding `target()`'s `1` into the call site.
- If the entry bytes are *not* NOPs, the flag didn't apply — check `RUSTC_BOOTSTRAP`/`RUSTFLAGS`.
- The 5-byte `E9 rel32` only reaches within ±2GB. Same-image (M1) is fine; cross-dylib (M2)
  needs an absolute jump or trampoline.
- Native `aarch64-apple-darwin` default `__TEXT` now works through page remapping, not through
  direct writes. Treat this as a page-granularity operation: production code must coordinate
  concurrent patching and avoid losing other edits on the same code page.
- Native `aarch64-apple-darwin` with `hot-segment-arm64` is also a real direct-call prologue
  patch, but only for functions deliberately placed in `__HOTRST`.
- If `hot-segment-arm64` is enabled without `-Clink-arg=-Wl,-segprot,__HOTRST,rwx,rwx`, the PoC
  fails before the first `target()` call instead of touching a misprotected segment.

## M2 (patch to freshly built dylib code)

`m2` proves that the replacement body does not have to be a sibling function in the same
image. At runtime it creates a temporary patch crate, spawns Cargo to build a `cdylib`, loads
the exported `hot_rust_m2_replacement` symbol, and patches the old `target()` entry to an
absolute jump into the dylib.

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m2
```

Verified locally on native Apple Silicon. Expected shape:

```
before patch: target() = 1
building patch crate with cargo...
hot_rust_m2_replacement() = 0x..., direct dylib call = 2
patch: 0x... -> 0x..., kind aarch64 ldr literal + br absolute, bytes [...]
code patch: direct write failed (...); frida-style remap copy succeeded
after  patch: target() = 2   (dylib replacement() itself = 2)
OK: direct call to target() now runs code loaded from the patch dylib.
```

The ARM64 cross-image stub uses all 16 patchable-entry bytes:

```
ldr x16, #8
br  x16
.quad replacement_address
```

That avoids the ±128MB range limit of a plain `B imm26` branch and is the shape M2 needs
before moving on to real reload artifacts.

## M3 (rust-analyzer change oracle)

`m3` is the first executable watcher/oracle slice. It uses rust-analyzer's `ra_ap_syntax`
parser/AST to compare old/new source snapshots, recover module-qualified item paths, and route
edits. It also uses `ra_ap_ide::Analysis::from_single_file(...).full_diagnostics(...)` as a
single-file validity gate before allowing a patch route.

```bash
RUSTC_BOOTSTRAP=1 \
RUSTFLAGS="-Zcrate-attr=feature(if_let_guard)" \
cargo run --features m3-oracle --bin m3
```

Expected output:

```
no_change: NoChange
body_only: BodyOnly path=render::paint export=hot_rust_patch_render_paint
signature_change: Structural reasons=["signature changed for render::paint"]
struct_layout_change: Structural reasons=["struct shape changed for Layout"]
invalid_source: Invalid errors=[...]
semantic_error: Invalid errors=["E0308: expected u32, found bool"]
OK: M3 oracle routes body-only edits to patch, structural edits to rebuild, and invalid edits to wait.
```

This is feature-gated as `m3-oracle` so M1/M2 builds do not pull rust-analyzer crates. Current
scope is single-file syntax/shape routing plus RA single-file diagnostics. Full Cargo graph
loading for arbitrary project crates is still the next hardening step.

## M4 (live symbol resolution)

`m4` proves the registration-table path for source-edit to running-binary identity. The live
process records `{ source_path, patch_export, signature_key, old_addr }` entries. An M3-style
intent for `render::paint` resolves to the old entry address, rejects stale path/export/signature
cases, and then feeds that resolved address into the same absolute-jump patcher shape used by
M2.

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m4
```

Expected shape:

```
live registry:
  render::paint export=hot_rust_patch_render_paint sig=extern "C" fn(u32) -> u32 old_addr=0x...
  render::stable export=hot_rust_patch_render_stable sig=extern "C" fn(u32) -> u32 old_addr=0x...
resolved render::paint -> old_addr=0x..., patch_export=hot_rust_patch_render_paint, sig=...
before patch: render::paint(10) = 11, render::stable(10) = 17
building patch crate with cargo...
hot_rust_patch_render_paint() = 0x..., direct dylib call = 110
patch: 0x... -> 0x..., kind aarch64 ldr literal + br absolute, bytes [...]
code patch: direct write failed (...); frida-style remap copy succeeded
after  patch: render::paint(10) = 110, render::stable(10) = 17, dylib replacement(10) = 110
OK: M4 resolved render::paint to the live entry and patched it through hot_rust_patch_render_paint.
```

This proves the engine no longer needs the old function pointer hardcoded at the call site:
the patcher receives an address resolved from source-level identity.
