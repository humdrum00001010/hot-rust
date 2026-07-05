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

## M5 (end-to-end native target harness)

`m5` composes the current proof chain against a target-shaped native render/layout function:

1. M3 classifies an old/new source edit to `layout::render_page_svg_native`.
2. M4 resolves the source path/export/signature to the old function's live entry address.
3. A patch `cdylib` is built with the requested export.
4. The M2/M4 absolute-jump patcher redirects the resolved old entry.
5. The next direct frame render observes the new layout body without restarting.

```bash
RUSTC_BOOTSTRAP=1 \
RUSTFLAGS="-Zpatchable-function-entry=16 -Zcrate-attr=feature(if_let_guard)" \
cargo run --features m3-oracle --bin m5
```

Expected shape:

```
target frame before edit: page=3 content=760x1060 bands=9
stable checksum before edit: 1923
M3 route: BodyOnly path=layout::render_page_svg_native export=hot_rust_patch_layout_render_page_svg_native
driver intent: source_path=layout::render_page_svg_native patch_export=hot_rust_patch_layout_render_page_svg_native signature=extern "C" fn(PageInput) -> LayoutMetrics
M4 resolved: layout::render_page_svg_native -> old_addr=0x..., export=hot_rust_patch_layout_render_page_svg_native, sig=extern "C" fn(PageInput) -> LayoutMetrics
building patch crate with cargo...
patch export hot_rust_patch_layout_render_page_svg_native = 0x..., direct dylib render = page=3 content=720x1040 bands=17
patch: 0x... -> 0x..., kind aarch64 ldr literal + br absolute, bytes [...]
code patch: direct write failed (...); frida-style remap copy succeeded
target frame after patch: page=3 content=720x1040 bands=17
stable checksum after patch: 1923
OK: M5 applied a body-only layout edit to the running native render target without restart.
```

This is still a harness, not a real external application integration. The next hardening step
is loading a full Cargo project and replacing the manual registry with reusable target-side
registration.

## M6 (`hr cargo ...` supervisor)

`hr` is the first service-shaped entrypoint. It runs before Cargo, starts a private
rust-analyzer LSP process configured for server-side file watching, prepares the hot-session
environment, injects `RUSTC_BOOTSTRAP=1` and `-Zpatchable-function-entry=16`, and then runs
Cargo.

Install the rust-analyzer rustup component if the local `rust-analyzer` command is only the
rustup proxy and the component is missing:

```bash
rustup component add rust-analyzer
```

Build the wrapper:

```bash
cargo build --bin hr
```

Pass-through Cargo commands:

```bash
target/debug/hr cargo --version
target/debug/hr cargo check --bin m1
```

`cargo run` is translated to `cargo build --message-format=json-render-diagnostics`; `hr`
parses the executable artifact and launches the target itself:

```bash
target/debug/hr cargo run --bin m1
```

Expected shape:

```
hr: hot env prepared before Cargo
hr: rust-analyzer initialized; requested server-side file watching
hr: translating cargo run -> cargo build --bin m1 --message-format=json-render-diagnostics
hr: launching .../target/debug/m1
OK: direct call to target() now runs replacement()'s body -- call site untouched.
hr: waiting for rust-analyzer to settle before shutdown
hr: ra status health=ok quiescent=true
```

For targets that need extra unstable flags, pass only the target-specific flag and let `hr`
append the patchable-entry flag. Use the built `hr` binary so the supervisor itself is not
rebuilt under target-only flags:

```bash
RUSTFLAGS="-Zcrate-attr=feature(if_let_guard)" \
target/debug/hr cargo run --features m3-oracle --bin m5
```

Verified locally on native Apple Silicon:

```
hr: rust-analyzer initialized; requested server-side file watching
hr: translating cargo run -> cargo build --features m3-oracle --bin m5 --message-format=json-render-diagnostics
hr: launching .../target/debug/m5
OK: M5 applied a body-only layout edit to the running native render target without restart.
hr: ra status health=ok quiescent=true
```

Live mode is opt-in through `HR_LIVE_SYMBOL`:

```bash
cargo build --bin hr --lib
HR_LIVE_SYMBOL=escape_xml \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 160
```

Current live mode is intentionally narrow. It discovers the selected binary source without a
path argument, refreshes source through rust-analyzer activity or an explicit rust-analyzer
symbol request, copies a same-signature edited function body into a temporary patch `cdylib`,
and sends a JSON patch command to the injected `libhr_runtime` Unix socket. The runtime resolves
the old symbol inside the main executable and patches from inside the process. Repatching the
same entry is supported by recognizing both original patch padding and the existing hot-jump
stub.

Verified against the real `rhwp` SVG renderer on `samples/aift.hwp` (5.5 MB, 74 pages):

```
hr: live runtime symbol escape_xml -> _RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml
hr: live source module hint renderer::svg
hr: live source symbol escape_xml uri file:///Users/phihu/Desktop/rhwp_core/src/renderer/svg.rs
hr: live source edit escape_xml body bytes 775 -> 1053
hr-runtime: patch applied old=0x100bf5c38 new=0x104394e34 symbol=_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml
hr: runtime patch OK old=0x100bf5c38 new=0x104394e34 symbol=_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml
HOT_RUST_RENDER_ESCAPE_XML_ONCE
hr: live source edit escape_xml body bytes 1053 -> 775
hr-runtime: patch applied old=0x100bf5c38 new=0x1043fcd04 symbol=_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml
hr: runtime patch OK old=0x100bf5c38 new=0x1043fcd04 symbol=_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml
```

This proves the service can start before Cargo, launch the actual target, observe an edit
through the rust-analyzer-backed loop, compile a patch, and change the already-running render
engine. It is not the final oracle yet: the configured-symbol path still needs to be replaced
with full-project M3 body-diff routing and broader ABI-aware patch codegen.

Worst-case check against the same real renderer:

```bash
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 80
```

Then edit the existing `SvgRenderer::render_node` body in `src/renderer/svg.rs`. This is a
large associated method (`fn render_node(&mut self, node: &RenderNode)`) that depends on
private renderer types, enum variants, module constants, `Self::` helpers, and other local
functions. The tiny standalone patch crate cannot compile this. The broad shadow-crate path now
copies the target crate, appends a same-module wrapper in `src/renderer/svg.rs`, compiles a
uniquely named `cdylib`, and patches the live method entry:

```
hr: live runtime symbol render_node -> _RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
hr: live source module hint renderer::svg::SvgRenderer
hr: live source symbol render_node uri file:///Users/phihu/Desktop/rhwp_core/src/renderer/svg.rs
hr: live source edit render_node body bytes 27330 -> 27617
hr: shadow patch crate ... source src/renderer/svg.rs impl SvgRenderer
Compiling hot_rust_shadow_patch_25182_1783186645475481000 v0.7.17 (...)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.61s
hr-runtime: patch applied old=0x101449f6c new=0x10a2db604 symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
hr: runtime patch OK old=0x101449f6c new=0x10a2db604 symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
HOT_RUST_RENDER_NODE_SHADOW_ONCE
hr: live source edit render_node body bytes 27617 -> 27330
Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.23s
hr-runtime: patch applied old=0x101449f6c new=0x10af9295c symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
hr: runtime patch OK old=0x101449f6c new=0x10af9295c symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
```

That proves the crude crate-context route works for the hard method. It is not yet the
cherry-picked dependency closure: the shadow copy is broad, and the compile is around 14s on
this machine. The next step is to use RA/rustc dependency information to shrink the shadow crate
instead of copying the whole target crate.

Experimental object probe:

```bash
HR_PATCH_BACKEND=object-probe \
HR_LIVE_SYMBOL=escape_xml \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 120
```

For a self-contained function body, `hr` now asks rustc for a relocatable object before falling
back to the existing dylib patch. The real `escape_xml` edit emitted a Mach-O object in 0.07s,
with the expected text/data symbols and ARM64 relocations:

```
hr: exact-fn object probe status=emitted elapsed=0.07s
hr: exact-fn object .../hot_rust_object_probe-....rcgu.o
hr: exact-fn object symbols:
hr:   ... (__TEXT,__text) ...
hr:   ... (__DATA,__bss) ... HOT_RUST_ESCAPE_XML_OBJECT_ONCE
hr: exact-fn object relocations:
hr:   Relocation information (__TEXT,__text) 84 entries
hr-runtime: patch applied old=0x105179c38 new=0x108804e34 symbol=_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml
HOT_RUST_ESCAPE_XML_OBJECT_PROBE
```

The same probe against the hard `SvgRenderer::render_node` method intentionally fails before
object emission when compiled outside the original module:

```
hr: exact-fn object probe status=compile-failed(exit status: 101) elapsed=25.75s
hr:   error[E0425]: cannot find function `escape_xml` in this scope
hr:   error[E0425]: cannot find value `TEXT_MARK_CLIP_RIGHT_PAD` in this scope
hr:   error[E0422]: cannot find struct, variant or union type `OverlayBounds` in this scope
hr:   error[E0616]: field `output` of struct `rhwp::renderer::svg::SvgRenderer` is private
hr:   error[E0624]: method `create_gradient_def` is private
```

So the model is directionally correct only after "exact function" means "exact body plus the
compiler's original crate/module context." For standalone bodies we can already get an object
quickly. For private module-heavy methods, the next real step is not dependency recompilation;
it is obtaining in-crate codegen for that one body, then installing the object by resolving and
applying the Mach-O relocations instead of using `dlopen`.

Dirty-CGU probe against the same hard method:

```bash
HR_PATCH_BACKEND=cgu-probe \
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 100000
```

Then edit the real `src/renderer/svg.rs::SvgRenderer::render_node` body. No wrapper, directive,
generated helper, or explicit path was provided. The live process detected the body edit and the
new backend found rustc's real incremental object for the already-running mangled symbol:

```
hr: live source edit render_node body bytes 27330 -> 27624
hr: dirty-CGU searching target objects for _RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node below /tmp/hot-rust-rhwp-target
hr: dirty-CGU running cargo rustc --bin rhwp -- -Z no-link
hr: dirty-CGU probe status=dirty-object-updated elapsed=5.29s target=/tmp/hot-rust-rhwp-target
hr: dirty-CGU before path=/tmp/hot-rust-rhwp-target/debug/incremental/rhwp-1mzxswr8nphdq/s-hk35xaq295-1vh3123-a9ue9uiz448txo1cbb22v7y4q/cawi17mqgizmnr04jfz1y2qga.o bytes=533984 mtime=1783187034.216886173 sha256=79b79ab5a32b8ec069c142424be2e302a23eec911b43e39a90aa58ff1971a748
hr: dirty-CGU after  path=/tmp/hot-rust-rhwp-target/debug/incremental/rhwp-1mzxswr8nphdq/s-hk35xvyu5g-0hu1kyk-2rk2y85ts7r3ur2fak0hrilgl/cawi17mqgizmnr04jfz1y2qga.o bytes=535176 mtime=1783189490.282813392 sha256=775a53d64e8dccc952b9550e2baa3e6e580761405d3dddcfb3093f3e37d829ac
hr: dirty-CGU compiler notes:
hr:   Compiling rhwp v0.7.17 (/Users/phihu/Desktop/rhwp_core)
hr:   Finished `dev` profile [unoptimized + debuginfo] target(s) in 5.26s
```

The old shadow fallback still ran afterward and proved the process was genuinely live, but it is
the slow path we want to remove:

```
Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.74s
hr-runtime: patch applied old=0x10330df6c new=0x10c35aedc symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
HOT_RUST_RENDER_NODE_CGU_PROBE
```

Restoring the same source body produced the original object hash again:

```
hr: live source edit render_node body bytes 27624 -> 27330
hr: dirty-CGU probe status=dirty-object-updated elapsed=4.73s target=/tmp/hot-rust-rhwp-target
hr: dirty-CGU before path=/tmp/hot-rust-rhwp-target/debug/incremental/rhwp-1mzxswr8nphdq/s-hk35xvyu5g-0hu1kyk-2rk2y85ts7r3ur2fak0hrilgl/cawi17mqgizmnr04jfz1y2qga.o bytes=535176 mtime=1783189490.282813392 sha256=775a53d64e8dccc952b9550e2baa3e6e580761405d3dddcfb3093f3e37d829ac
hr: dirty-CGU after  path=/tmp/hot-rust-rhwp-target/debug/incremental/rhwp-1mzxswr8nphdq/s-hk35yhdrth-0uns5o9-a9ue9uiz448txo1cbb22v7y4q/cawi17mqgizmnr04jfz1y2qga.o bytes=533984 mtime=1783189526.282737228 sha256=79b79ab5a32b8ec069c142424be2e302a23eec911b43e39a90aa58ff1971a748
hr:   Finished `dev` profile [unoptimized + debuginfo] target(s) in 4.70s
```

This changes the next step. For `render_node`, dependency cherry-picking is probably not the
first bottleneck to attack. rustc already emits the correct in-crate CGU object in around 5s;
the missing part is a target-side Mach-O object loader that maps that object, applies ARM64
relocations, resolves extern symbols against the running images, and patches the old entry to
the loaded symbol address.

First dirty-CGU object installer:

`HR_PATCH_BACKEND=cgu` now sends the dirty CGU object to `libhr_runtime` instead of using the
shadow dylib fallback. The runtime parses the relocatable Mach-O object, maps the loadable
`__TEXT` / `__DATA` sections, resolves object-local symbols, resolves Rust-private externs from
the running executable's Mach-O symbol table, applies the ARM64 relocations used by the
`render_node` CGU, creates branch stubs for far `BRANCH26` calls, and patches the old method
entry to the loaded object symbol.

The first attempt with a one-shot atomic marker correctly exposed the dependency boundary:

```
hr-runtime: command failed: process symbol __RNvMs3_NtNtCsezyhsC6CBT9_4core4sync6atomicNtB5_10AtomicBool4swapCs9fDSol6aaUk_4rhwp not found in /private/tmp/hot-rust-rhwp-target/debug/rhwp
```

That failure is useful: a dirty object that introduces a new monomorphization absent from the
old executable needs the fake compiler/loader to include that dependency too, or the edit has to
stay within symbols already present in the running binary.

A second real `render_node` edit used a direct stderr marker and installed successfully:

```
hr: live source edit render_node body bytes 27330 -> 27391
hr: dirty-CGU running cargo rustc --bin rhwp -- -Z no-link
hr: dirty-CGU probe status=dirty-object-updated elapsed=4.15s target=/tmp/hot-rust-rhwp-target
hr: dirty-CGU after  path=/tmp/hot-rust-rhwp-target/debug/incremental/rhwp-1mzxswr8nphdq/s-hk36knd2zn-1hrkkma-8qg7913rtn04ehhf7ipcr1j0z/cawi17mqgizmnr04jfz1y2qga.o bytes=534232 mtime=1783190866.189106824 sha256=f111050e77eaeaf2a3e3e02a523fc8094b8c198d611db37aa06b71d783e4f749
hr-runtime: object loaded path=/tmp/hot-rust-rhwp-target/debug/incremental/rhwp-1mzxswr8nphdq/s-hk36knd2zn-1hrkkma-8qg7913rtn04ehhf7ipcr1j0z/cawi17mqgizmnr04jfz1y2qga.o base=0x106d10000 size=212992 entry=0x106d10000 relocations=4729 stubs=2
hr-runtime: patch applied old=0x1036d9f6c new=0x106d10000 symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
hr: runtime object patch OK old=0x1036d9f6c new=0x106d10000 symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
HOT_RUST_RENDER_NODE_CGU_OBJECT_APPLY
```

This is the first proof that a large real `rhwp` method can be swapped from rustc's own dirty
incremental object without a wrapper, source directive, generated helper, shadow crate, final
executable link, or target restart. The current loader is intentionally narrow: macOS arm64,
debug-shaped CGU objects, no unwind registration, and only the relocation forms observed in the
real `render_node` object.

CGU-count experiment:

`hr` accepts `HR_CODEGEN_UNITS=N`, which appends `-Ccodegen-units=N` while preserving caller
`RUSTFLAGS` and the required patchable-entry flag. This is also possible manually with
`RUSTFLAGS="-Ccodegen-units=N"`, but the env knob keeps hot-reload runs reproducible.

Against real `rhwp`, a cold target built with `N=1024` raised the main crate's incremental object
count from 256 to 971:

```
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-cgu1024 \
RUSTC_BOOTSTRAP=1 \
RUSTFLAGS="-Zpatchable-function-entry=16 -Ccodegen-units=1024" \
cargo rustc --bin rhwp -- -Z no-link
```

That did not improve the hard target. The `SvgRenderer::render_node` object stayed the same
533,984-byte `cawi17mqgizmnr04jfz1y2qga.o` with the same SHA-256
`79b79ab5a32b8ec069c142424be2e302a23eec911b43e39a90aa58ff1971a748`, and still carried 57
`(__TEXT,__text)` symbols from the same SVG renderer cluster. So increasing CGU count is useful as
a knob, but it is not by itself the path to single-function granularity for `render_node`.

The same conclusion held inside the generated fake crate. A warmed body edit was about 1.64s with
the default partitioning, about 1.57s with final-crate-only `-Ccodegen-units=16`, and about 1.67s
with `-Ccodegen-units=512`. The bottleneck had moved to the fake crate's remaining type/surface
inputs, not LLVM partitioning.

Shadow-stub dylib proof:

`HR_PATCH_BACKEND=shadow-stub` keeps the current shadow `cdylib` route but rewrites selected
helper calls inside the edited method body to generated exported stubs. The runtime `dlopen`s the
dylib, patches each stub entry back to the old executable's real helper symbol, and only then
patches the old method entry to the new dylib code. The first default stub set for
`render_node` is `escape_xml,color_to_svg`; it can be overridden with `HR_SHADOW_STUBS`.

Real run:

```bash
HR_PATCH_BACKEND=shadow-stub \
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
/Users/phihu/Desktop/hot-rust/poc/target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 100000
```

Then edit the real `src/renderer/svg.rs::SvgRenderer::render_node` body. The process discovered
the source without an explicit path, built the shadow-stub dylib, patched both generated stubs to
old executable symbols, patched `render_node`, and the running renderer observed the edit:

```
hr: live source edit render_node body bytes 27330 -> 27385
hr: shadow-stub patch crate ... source src/renderer/svg.rs impl SvgRenderer stubs=hot_rust_stub_escape_xml->_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml,hot_rust_stub_color_to_svg->_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg12color_to_svg
Finished `dev` profile [unoptimized + debuginfo] target(s) in 14.93s
hr-runtime: stub patched stub=hot_rust_stub_escape_xml at 0x10a85b558 -> _RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml at 0x101931c38
hr-runtime: stub patched stub=hot_rust_stub_color_to_svg at 0x10a85b4e8 -> _RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg12color_to_svg at 0x101931f40
hr-runtime: patch applied old=0x10191df6c new=0x10a85b47c symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
hr: runtime patch OK old=0x10191df6c new=0x10a85b47c symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
HOT_RUST_RENDER_NODE_SHADOW_STUB
```

This proves the dependency-stub direction against the same large real renderer method. It does
not yet synthesize ABI shells or method stubs; the first implementation still copies the crate for
layout/type correctness and stubs only selected same-module free-function helpers.

Shadow-mini prune proof:

`HR_PATCH_BACKEND=shadow-mini` is the first compile-input shrink on top of `shadow-stub`. It still
copies the crate to preserve type/layout/privacy correctness, but it strips unrelated function
bodies in the shadow copy to `unimplemented!()` before compiling the patch dylib. For the current
`render_node` path it preserves `src/renderer`, `src/model`, `src/paint`, `src/ole_chart`, and
`src/ooxml_chart` so the live renderer can keep executing real helper behavior; the rest of the
crate is reduced to body stubs. The preserve set can be overridden with
`HR_SHADOW_PRESERVE_PREFIXES`.

The first two attempts exposed parser hardening issues in the pruner:

- plain token scanning matched `fn` inside strings/comments;
- brace matching counted braces inside strings and initially treated Rust labels like `'outer:`
  as char literals.

After adding string/comment/raw-string aware token and brace scanning, the real `render_node` edit
compiled and patched:

```bash
HR_PATCH_BACKEND=shadow-mini \
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
/Users/phihu/Desktop/hot-rust/poc/target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 100000
```

Observed output:

```
hr: live source edit render_node body bytes 27330 -> 27649
hr: shadow-mini pruned 4067 function bodies across 247 files; preserved prefixes=src/renderer,src/model,src/paint,src/ole_chart,src/ooxml_chart
hr: shadow-stub patch crate ... source src/renderer/svg.rs impl SvgRenderer stubs=hot_rust_stub_escape_xml->_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml,hot_rust_stub_color_to_svg->_RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg12color_to_svg
Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.75s
hr: shadow-stub build elapsed=8.78s
hr-runtime: stub patched stub=hot_rust_stub_escape_xml at 0x10c618400 -> _RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg10escape_xml at 0x1036bdc38
hr-runtime: stub patched stub=hot_rust_stub_color_to_svg at 0x10c618390 -> _RNvNtNtCs9fDSol6aaUk_4rhwp8renderer3svg12color_to_svg at 0x1036bdf40
hr-runtime: patch applied old=0x1036a9f6c new=0x10c618324 symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
hr: runtime patch OK old=0x1036a9f6c new=0x10c618324 symbol=_RNvMNtNtCs9fDSol6aaUk_4rhwp8renderer3svgNtB2_11SvgRenderer11render_node
HOT_RUST_RENDER_NODE_SHADOW_MINI
```

This is still not the final fake compiler: it does not synthesize type shells, and it keeps broad
renderer/model/paint source prefixes intact. But it proves that pruning unrelated copied behavior
can cut the real hard-method shadow compile from roughly 15s to 8.78s while preserving live
execution against the actual renderer.

Shadow-fake compile-time pass:

After the live proof, the useful optimization target was the generated patch crate itself. `hr`
now has `HR_PATCH_BUILD_ONLY=1`, which builds the real cargo target to obtain symbols and source
mapping, then generates/builds the patch crate without launching or patching the renderer.

Baseline command:

```bash
HR_PATCH_BUILD_ONLY=1 \
HR_PATCH_BACKEND=shadow-mini \
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
/Users/phihu/Desktop/hot-rust/poc/target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 100000
```

Baseline output:

```text
hr: patch build-only render_node signature `fn render_node(&mut self, node: &RenderNode)` body-bytes=27330
hr: shadow-mini pruned 4067 function bodies across 247 files; preserved prefixes=src/renderer,src/model,src/paint,src/ole_chart,src/ooxml_chart mode=outside-preserved
Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.54s
hr: shadow-stub build elapsed=8.57s
```

`HR_PATCH_BACKEND=shadow-fake` keeps the real `render_node` body but treats the rest of the shadow
crate more like a compiler artifact:

- direct same-`impl` calls from `render_node` are rewritten to generated exported method stubs;
- runtime would patch those stubs back to the old executable's methods, same as free-function stubs;
- the target `src/renderer/svg.rs` prunes every non-live function body;
- the copied crate prunes function bodies even inside the preserved renderer/model/paint surface;
- unused serde derive entries and helper attrs are stripped in the fake crate;
- generated patch crates build with `CARGO_INCREMENTAL=0`, because package names are unique and
  incremental caches are mostly write overhead.

Final build-only command:

```bash
HR_PATCH_BUILD_ONLY=1 \
HR_KEEP_PATCH_ROOT=1 \
HR_PATCH_BACKEND=shadow-fake \
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
/Users/phihu/Desktop/hot-rust/poc/target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 100000
```

Final output:

```text
hr: patch build-only render_node signature `fn render_node(&mut self, node: &RenderNode)` body-bytes=27330
hr: shadow-fake exported 16 method stubs from src/renderer/svg.rs
hr: shadow-fake pruned 37 non-live function bodies in src/renderer/svg.rs
hr: shadow-mini pruned 6937 function bodies across 339 files; preserved prefixes=src/renderer,src/model,src/paint,src/ole_chart,src/ooxml_chart mode=all
hr: shadow-fake stripped 97 serde derive entries and 74 serde attrs across 21 files
Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.78s
hr: shadow-stub build elapsed=2.82s
```

Fresh rustc phase profile for the final generated crate, after clearing only that package's target
artifacts and using `CARGO_INCREMENTAL=0`:

```text
total rustc:              2.881s
type_check_crate:         0.846s
MIR_borrow_checking:      0.573s
macro_expand_crate:       0.473s
codegen_to_LLVM_IR:       0.212s
LLVM_passes:              0.258s
generate_crate_metadata:  0.181s
link:                     0.076s
```

This is a compile-time result only. The previous live attempt proved the marker could patch, but
then hit a pruned body because the old function extractor matched `draw_line_shape` before
`draw_line`. That extractor has since been fixed to require exact function-name boundaries, but
the latest pass intentionally stopped at build-only timing.

Persistent fake crate experiment:

The next experiment keeps the fake crate path/package stable and copies the built dylib to a
unique path for future `dlopen` calls. This lets rustc reuse incremental state for the generated
shell/stub surface while still avoiding dynamic-loader image reuse.

Command:

```bash
HR_PATCH_BUILD_ONLY=1 \
HR_SHADOW_PERSISTENT=1 \
HR_KEEP_PATCH_ROOT=1 \
HR_PATCH_BACKEND=shadow-fake \
HR_LIVE_SYMBOL=render_node \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
/Users/phihu/Desktop/hot-rust/poc/target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 100000
```

Cold persistent setup, after clearing only the generated fake-crate cache:

```text
hr: shadow-fake persistent crate /tmp/hot-rust-rhwp-target/hot-rust-shadow-fake/render_node-2cf091917df3ad59 synced files=8986 bytes=619835041 lib=hot_rust_shadow_fake_render_node_2cf091917df3ad59
Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.76s
hr: shadow-stub build elapsed=3.79s
```

Second run with unchanged source, proving the stable crate can be reused:

```text
hr: shadow-fake persistent crate ... synced files=1 bytes=46055 lib=hot_rust_shadow_fake_render_node_2cf091917df3ad59
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.04s
hr: shadow-stub build elapsed=0.07s
```

Then a real body-only edit was made in `rhwp_core/src/renderer/svg.rs::render_node`:

```rust
let _hot_rust_compile_time_probe = "HOT_RUST_PERSISTENT_FAKE_COMPILE";
```

Build-only result for the edited body:

```text
hr: patch build-only render_node signature `fn render_node(&mut self, node: &RenderNode)` body-bytes=27409
hr: shadow-fake persistent crate ... synced files=2 bytes=98659 lib=hot_rust_shadow_fake_render_node_2cf091917df3ad59
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.65s
hr: shadow-stub build elapsed=1.68s
```

Restoring that edit measured similarly:

```text
hr: patch build-only render_node signature `fn render_node(&mut self, node: &RenderNode)` body-bytes=27330
hr: shadow-fake persistent crate ... synced files=2 bytes=98580 lib=hot_rust_shadow_fake_render_node_2cf091917df3ad59
Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.64s
hr: shadow-stub build elapsed=1.67s
```

Caveat: the current persistent implementation still generates a full staging tree before syncing
changed bytes into the stable crate, so total build-only command wall time is not the hot-loop
target yet. The compile step itself dropped from 2.82s disposable fake crate to about 1.67s for a
real edited body after the persistent crate was warm. The obvious next implementation cut is to
update only the already-transformed persistent target file instead of rebuilding the staging tree.

Persistent fake crate prune:

Two more cuts were added after the persistent experiment:

- fake-shadow staging now copies only Cargo/build files, `src`, `examples`, `.cargo`, and the tiny
  fixtures still referenced by pruned compile-time include paths instead of cloning the whole
  working tree;
- the generated fake crate root is rewritten to expose only `error`, `model`, `ole_chart`,
  `ooxml_chart`, `paint`, and `renderer`, with narrow stubs for parser/serializer/document-core
  names still referenced by the kept surface.

That keeps rustc focused on the render/type surface instead of unused crate modules.

Unchanged real source, with the persistent fake crate already warm:

```text
hr: shadow-fake persistent crate /tmp/hot-rust-rhwp-target/hot-rust-shadow-fake/render_node-2cf091917df3ad59 synced files=2 bytes=98580 lib=hot_rust_shadow_fake_render_node_2cf091917df3ad59
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.83s
hr: shadow-stub build elapsed=0.86s
```

Then a real body-only edit was made in `rhwp_core/src/renderer/svg.rs::render_node`:

```rust
let _hot_rust_pruned_fake_probe = 1usize;
```

Build-only result for the edited body:

```text
hr: patch build-only render_node signature `fn render_node(&mut self, node: &RenderNode)` body-bytes=27380
hr: shadow-fake persistent crate /tmp/hot-rust-rhwp-target/hot-rust-shadow-fake/render_node-2cf091917df3ad59 synced files=2 bytes=98630 lib=hot_rust_shadow_fake_render_node_2cf091917df3ad59
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.93s
hr: shadow-stub build elapsed=0.96s
```

The probe edit was restored immediately after the measurement. The preliminary build-only command
still runs `cargo build --bin rhwp` to find the executable and symbol, so the total command wall
time is not the service hot-loop target. The measured patch-crate step is the relevant number for
the fake-compiler branch.

Post-prune bottleneck remeasure:

`HR_TIMING=1` adds gated timing logs around the build-only service path, real target build, source
discovery, fake-crate generation, persistent sync, and patch Cargo build.

Before caching executable symbols, a real body-only `render_node` edit showed that rustc was no
longer the bottleneck:

```text
hr: timing target-cargo-build elapsed=3.747s
hr: timing build-only-symbol-resolve elapsed=0.719s
hr: timing build-only-source-discovery elapsed=3.025s
hr: timing shadow-free-stubs elapsed=1.331s
hr: timing shadow-method-stubs elapsed=10.727s
hr: timing shadow-tree-prune elapsed=0.605s
hr: timing shadow-cargo-build elapsed=0.812s
hr: shadow-stub build elapsed=0.81s
hr: timing shadow-total elapsed=13.604s
hr: timing total elapsed=21.122s
```

Root cause: every generated stub lookup called `binary_symbol_for`, and that function spawned
`nm` over the large `rhwp` executable and ran `cargo metadata`. `render_node` currently exports
2 free-function stubs and 16 same-impl method stubs, so repeated symbol scans dominated the path.

`BinarySymbolResolver` now builds one executable text-symbol index per patch build and reuses it
for free-function and method-stub lookups. Repeating the measurement on a forced restore edit:

```text
hr: timing target-cargo-build elapsed=3.786s
hr: timing build-only-symbol-resolve elapsed=0.953s
hr: timing build-only-source-discovery elapsed=2.771s
hr: timing shadow-symbol-index elapsed=0.844s
hr: timing shadow-free-stubs elapsed=0.037s
hr: timing shadow-method-stubs elapsed=0.345s
hr: timing shadow-tree-prune elapsed=0.607s
hr: timing shadow-cargo-build elapsed=0.826s
hr: shadow-stub build elapsed=0.83s
hr: timing shadow-total elapsed=2.790s
hr: timing total elapsed=10.344s
```

That total still includes build-only harness scaffolding. For the actual hot loop, the target is
already built/running, the source file is already known from the service's edit stream, and
executable symbols are stable until a rebuild. Those are session setup costs, not per-edit costs.

The service now builds one `BinarySymbolResolver` when the executable is known and passes it into
the patch builder. With that cache reused, a real body-only `render_node` edit measured:

```text
hr: timing shadow-symbol-index-reuse elapsed=0.000s
hr: timing shadow-stage-copy elapsed=0.038s
hr: timing shadow-free-stubs elapsed=0.036s
hr: timing shadow-method-stubs elapsed=0.341s
hr: timing shadow-tree-prune elapsed=0.607s
hr: timing shadow-serde-strip elapsed=0.049s
hr: timing shadow-persistent-sync elapsed=0.016s
hr: timing shadow-cargo-build elapsed=0.917s
hr: shadow-stub build elapsed=0.92s
hr: timing shadow-total elapsed=2.031s
```

At that point, the corrected per-edit bottleneck was the fake patch generator, not the target repo
build. Inside that hot loop, the largest buckets were patch rustc (~0.9s), whole-tree prune
(~0.6s), and method-stub rewriting (~0.34s). The next useful cut was not CGU count or target
rebuild avoidance; it was keeping the transformed fake crate warm and updating only the
changed/generated live file, plus caching the direct-callee stub plan for unchanged function shape.

Stub-first hot path:

The persistent fake crate now treats the broad tree prune as skeleton initialization, not edit
work. If the generated crate already has `Cargo.toml` and `src/lib.rs`, `shadow-fake` skips source
copying, whole-tree pruning, serde stripping, crate-root rewriting, and persistent-tree sync. It
reads the real live source, transforms only that file, writes it into the persistent fake crate,
and rebuilds the stable fake crate.

Real body-only edit:

```text
hr: shadow-fake persistent skeleton reused
hr: timing shadow-free-stubs elapsed=0.037s
hr: timing shadow-method-stubs elapsed=0.350s
hr: timing shadow-target-prune elapsed=0.004s
hr: timing shadow-tree-prune-skip elapsed=0.000s
hr: timing shadow-serde-strip-skip elapsed=0.000s
hr: timing shadow-persistent-sync-skip elapsed=0.000s
hr: timing shadow-cargo-build elapsed=0.989s
hr: shadow-stub build elapsed=0.99s
hr: timing shadow-total elapsed=1.385s
```

Restore edit:

```text
hr: shadow-fake persistent skeleton reused
hr: timing shadow-free-stubs elapsed=0.037s
hr: timing shadow-method-stubs elapsed=0.350s
hr: timing shadow-target-prune elapsed=0.004s
hr: timing shadow-tree-prune-skip elapsed=0.000s
hr: timing shadow-cargo-build elapsed=0.972s
hr: shadow-stub build elapsed=0.97s
hr: timing shadow-total elapsed=1.369s
```

This is now much closer to the intended model: all non-hot code lives as stubs in the persistent
fake skeleton, and only the changed live body is made real per edit. The remaining hot-path work is
mostly patch rustc (~1.0s) and repeated direct-callee/method-stub planning (~0.35s).

A final-crate rustc phase sample on the generated fake crate, forced with
`cargo rustc --lib -- -Ztime-passes`, reported:

```text
total rustc:              0.730s
macro_expand_crate:       0.143s
generate_crate_metadata:  0.096s
link:                     0.072s
type_check_crate:         0.035s
MIR_borrow_checking:      0.019s
codegen_to_LLVM_IR:       0.006s
LLVM_passes:              0.022s
```

At this point, more fake-crate compile-time work has diminishing returns unless it also removes
the remaining symbol indexing, pruning pass, or module surface that causes macro expansion and
metadata emission.
