# hot-rust

`hot-rust` is a native live-reload CLI for Rust development.

Run your app through `hr`, edit one function body, and the running process can pick up the
new body without restarting the app or rebuilding the whole crate.

```bash
cd your-rust-project
hr cargo run
```

## Current Support

This is an alpha release path.

- macOS Apple Silicon is the tested target.
- Windows is not a live-patch target yet. `hr cargo check` / `hr cargo run` can
  still act as a Cargo wrapper, but runtime patching is disabled; see
  [docs/WINDOWS.md](docs/WINDOWS.md).
- Rust native binaries only. WebAssembly is not supported.
- Debug/dev builds are the intended workflow.
- Body-only function edits are patchable.
- Signature, type, struct-layout, trait, macro, or multiple-function edits require a restart.
- `rust-analyzer` must be installed and available in `PATH`.

## Install

Download the installer from the latest GitHub Release:

```bash
curl -LO https://github.com/humdrum00001010/hot-rust/releases/latest/download/install.sh
sh install.sh
```

This downloads `install.sh` as a local file first, then runs that local file.

The installer downloads the matching release bundle and installs:

```text
~/.local/lib/hot-rust/hr
~/.local/lib/hot-rust/libhr_runtime.dylib
~/.local/bin/hr
```

Windows assets currently install `hr.exe` as a Cargo wrapper. They do not include
the live-patch runtime yet.

If `~/.local/bin` is not already in `PATH`, the installer adds it to your shell profile.
Open a new terminal after install if `hr` is not found immediately.

## Usage

Use `hr` as a Cargo wrapper:

```bash
hr cargo run
hr cargo run --bin app
hr cargo run --bin app -- arg1 arg2
hr cargo check
```

For live reload, keep the process running and edit a function body in the project. `hr` watches
the project through rust-analyzer, builds a small patch artifact, and installs it into the running
process.

Before installing a patch, `hr` checks rust-analyzer's lightweight diagnostics for the changed
function, compiles the patch artifact, and asks the runtime to validate patch loading, symbols,
and entry bytes. It does not block the hot path on a full `cargo check`.

Normal usage does not require `HR_LIVE_SYMBOL`, `HR_PATCH_BACKEND`, or runtime path variables.
Those are diagnostic/development overrides.

## What To Expect

Patchable edit:

```rust
fn label() -> &'static str {
    "before"
}
```

Change only the body:

```rust
fn label() -> &'static str {
    "after"
}
```

The running process should switch on the next call to `label`.

Rebuild-required edit:

```rust
fn label(verbose: bool) -> &'static str {
    if verbose { "after" } else { "before" }
}
```

That changes the signature, so `hr` refuses to patch it and tells you a restart is required.

## Requirements

Install Rust and rust-analyzer first:

```bash
rustup component add rust-analyzer
```

`hr` injects the required compiler flag into the wrapped Cargo command:

```text
RUSTC_BOOTSTRAP=1
RUSTFLAGS+=-Zpatchable-function-entry=16
```

You do not need to set those yourself.

## Troubleshooting

If `hr` is not found:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

If `rust-analyzer` is not found:

```bash
rustup component add rust-analyzer
```

If live reload does not happen, check the terminal output. Common reasons are:

- the edit changed a signature or type/layout boundary
- more than one function body changed at once
- the target process exited before the patch was built
- the project is being built in release/optimized mode where functions may be inlined

## Uninstall

```bash
rm -rf ~/.local/lib/hot-rust
rm -f ~/.local/bin/hr
```

Remove the `hot-rust` PATH block from your shell profile if the installer added one.

## For Maintainers

Build release assets:

```bash
./scripts/package-release.sh
HOT_RUST_TARGET=x86_64-pc-windows-msvc ./scripts/package-release.sh
```

Upload these files to the GitHub Release:

```text
install.sh
dist/hot-rust-aarch64-apple-darwin.tar.gz
dist/hot-rust-aarch64-apple-darwin.tar.gz.sha256
dist/hot-rust-x86_64-pc-windows-msvc.tar.gz
dist/hot-rust-x86_64-pc-windows-msvc.tar.gz.sha256
```

## Details

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains the runtime patching model.
- [docs/NOTES.md](docs/NOTES.md) has local development and release notes.
- [docs/ROADMAP.md](docs/ROADMAP.md) tracks current limitations and next work.
- [docs/RESEARCH.md](docs/RESEARCH.md) records the tool landscape and rationale.
- [docs/WINDOWS.md](docs/WINDOWS.md) covers Windows cross-target packaging and install usage.
