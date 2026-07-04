# hot-rust

A native, function-level **hot-patch engine for Rust** — change a function's body in a
running program and have it take effect *without a restart or a full rebuild*. It's Live++'s
mechanism, rebuilt in Rust, on top of compiler support that already ships.

## Why this exists

Rust's edit→run loop is slow (motivating case: the `rhwp` HWP viewer, ~20-min release
builds). We surveyed the field and nothing fits the exact need — *transparent, innermost,
native, function-level* hot-patch for Rust:

| tool | verdict for us |
|---|---|
| **subsecond** (Dioxus) | works & maintained, but **experimental**, needs intrusive `subsecond::call` sites, uses a *jump-table* technique. The only **wasm**-capable option. |
| **Live++** | the gold standard — but **native/Windows-only and does not support Rust** (roadmap "under investigation" since 2022). |
| **hot-lib-reloader** | mature-ish but **stale** (last release 2025-08); native dylib-swap, not transparent, has gotchas (`tracing`, TypeId). |
| **fork rustc** | the compiler knob we'd add (`-Zpatchable-function-entry`) **already ships since 1.81** → forking is pointless. |

So we build the missing piece: the **engine** that consumes the existing compiler flag.

## The architecture in one paragraph

Three parties, each doing only what it's good at:

1. **rust-analyzer — as a *watcher*, not a compiler.** The change oracle: it says *which
   function's body* changed, *whether it's safe to patch* (body-only) vs needs a rebuild
   (signature / type / struct-layout / macro), and only fires when the edit type-checks.
2. **rustc — codegen.** Compiles just the changed function into patch machine code.
3. **the engine (this repo) — the patcher.** Overwrites the old function's entry padding
   (emitted by `-Zpatchable-function-entry`) with a jump to the new code, in the running
   process. Live++-style prologue trampoline.

```
edit → rust-analyzer (what changed? patchable?) → rustc codegen(just that fn) → engine writes the jump
         └─ oracle + router ─┘                     └─ the actual patch bytes ─┘   └─ M1..M5 ─┘
```

## Status (at this capture)

- Architecture + landscape: **understood** (this directory).
- `-Zpatchable-function-entry`: **confirmed** in stock rustc since **1.81** (unstable; the
  general LLVM/GCC `-fpatchable-function-entry` primitive). The Windows-only `-Zhotpatch`
  sugar is separate and was *not* in the checkout we inspected.
- **M1** (prove the in-place prologue jump): **designed, not yet built** — see `poc/`.
- **M2–M5**: specced (see `ROADMAP.md`).

## Hard boundaries (do not forget)

- **Native only.** wasm code is immutable — you cannot patch a prologue in a browser. This
  engine will never work on wasm; that's subsecond's jump-table territory.
- **Body-only.** Signature / struct-layout / type changes cannot be patched — the old state
  in memory would be misinterpreted. These require a rebuild. (rust-analyzer is what detects
  and routes them — that's the safety gate.)
- **No state migration.** Patching code does not undo already-run code or migrate globals.
- **No inlining.** Build with inlining off; an inlined function has no distinct entry to patch.

## Read next

- `ARCHITECTURE.md` — how the engine + the 3-party pipeline work, in detail.
- `ROADMAP.md` — M1..M5 milestones and what each proves.
- `RESEARCH.md` — the full landscape, the decisions and their rationale, and sources.
- `poc/` — the M1 starter (unbuilt).
