# hot-rust notes

## Build

```bash
cargo build --bin hr --lib
```

`hr` injects the hot-patch compiler flag into target Cargo commands:

```text
RUSTC_BOOTSTRAP=1
RUSTFLAGS+=-Zpatchable-function-entry=16
```

## Basic Service Usage

```bash
target/debug/hr cargo check
target/debug/hr cargo run --bin <target> -- <args>
```

For `cargo run`, `hr` translates the command to a JSON-emitting Cargo build, finds the
executable artifact, injects `libhr_runtime` when it is available next to `hr`, then launches the
target itself so the runtime patch socket remains tied to the target process.

## Live Mode

Live mode is the default production path for `cargo run`:

```bash
cargo build --bin hr --lib
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 160
```

`hr` boots `RustAnalyzerDriver` first. The driver starts rust-analyzer and requests server-side
project watching before Cargo or the target process are launched. Live mode snapshots the project
functions, waits for rust-analyzer activity, infers a single body-only function edit, builds a
patch artifact, and sends a JSON patch command to the target-side `libhr_runtime` Unix socket.
`HR_LIVE_SYMBOL` is retained only as a debug override to force a single function.

## Real rhwp Worst-Case

The current worst-case benchmark is `SvgRenderer::render_node` in `rhwp_core`:

```bash
HR_TIMING=1 \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 1
```

Build-only smoke:

```bash
HR_TIMING=1 \
HR_LIVE_SYMBOL=render_node \
HR_PATCH_BUILD_ONLY=1 \
CARGO_TARGET_DIR=/tmp/hot-rust-rhwp-target \
target/debug/hr cargo run --bin rhwp -- bench samples/aift.hwp -n 1
```

Recent measured shape after the refactor:

```text
shadow-xref-prewarm ~= 1.15s
shadow-method-stubs ~= 0.34s
shadow-external-method-stubs ~= 0.05s
shadow-external-function-stubs ~= 0.04s
shadow-cargo-build ~= 0.74s
shadow-total ~= 1.21s
total with rust-analyzer/source discovery ~= 6.52s
```

## Backend Labels

- `shadow-fake`: default measured large-method path.
- `shadow-stub`: legacy full shadow-stub baseline.
- `shadow-mini`: legacy pruned shadow baseline.
- `object-probe`: diagnostic exact-function object emission.
- `cgu-probe`: diagnostic dirty-CGU object emission.
- `object`: dead-end placeholder until object relocation is wired.
- `cgu`: experimental runtime object-loader route.

## Current Boundaries

- Body-only patches are the intended route.
- Signature/type/layout changes must rebuild.
- Multiple changed function bodies in one observed edit are currently routed to rebuild.
- Bad patch artifacts can still crash the target process; crash containment and rollback are
  future work.
- Inlining must stay off for a distinct patchable entry to exist.
