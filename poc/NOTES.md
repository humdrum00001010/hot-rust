# poc — build & run

## M1 (in-place prologue jump)

Windows / x86-64. Needs the unstable `-Zpatchable-function-entry` flag, unlocked on a stable
toolchain via `RUSTC_BOOTSTRAP=1`:

```powershell
$env:RUSTC_BOOTSTRAP = "1"
$env:RUSTFLAGS = "-Zpatchable-function-entry=16"
cargo run --bin m1
```

or bash:

```bash
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1
```

### Expected output (roughly)
```
target() entry bytes (expect NOP padding): [0f, 1f, ...]   # or 90 90 ... — proves the flag
before patch: target() = 1
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
