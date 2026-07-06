# Windows Cross-Target Usage

Windows support is currently a release/install usage layer, not a live-patch
runtime implementation.

The Windows asset installs `hr.exe` as a Cargo wrapper:

```powershell
hr cargo check
hr cargo run
```

It does not ship `hr_runtime.dll`, and runtime live patching is not claimed for
Windows yet.

## Build A Windows Asset

Install the Rust target first:

```bash
rustup target add x86_64-pc-windows-msvc
```

Then package with an explicit target:

```bash
HOT_RUST_TARGET=x86_64-pc-windows-msvc ./scripts/package-release.sh
```

The script routes Cargo and file collection through the selected triple and
emits:

```text
dist/hot-rust-x86_64-pc-windows-msvc.tar.gz
dist/hot-rust-x86_64-pc-windows-msvc.tar.gz.sha256
```

The host still needs a working linker/toolchain for the selected Windows target.
For a cheap CI gate that avoids linking, use:

```bash
cargo check --target x86_64-pc-windows-msvc --bin hr
```

## Install On Windows

From Git Bash or an equivalent POSIX shell:

```bash
curl -LO https://github.com/humdrum00001010/hot-rust/releases/latest/download/install.sh
sh install.sh
```

On Windows-like shells, `install.sh` resolves the release asset as
`hot-rust-x86_64-pc-windows-msvc.tar.gz`, installs `hr.exe`, and writes an `hr`
shell wrapper. When `cygpath` is available, it also writes `hr.cmd`.

To force the asset triple explicitly:

```bash
sh install.sh --target x86_64-pc-windows-msvc
```
