# Research, decisions, and sources

The landscape we surveyed before deciding to build, and *why* each decision fell the way it did.

## The landscape (2026)

### subsecond (Dioxus)
- Runtime hot-patching engine for Rust. **Maintained** (updated 2026-05, part of active Dioxus
  monorepo) but **explicitly experimental**, dev-only.
- Technique: **jump table** at explicit `subsecond::call()` sites (intrusive). Recompiles
  changed fns via **ThinLink** (a custom linker that lives *inside* the `dx` CLI, not
  standalone), links against the running program's addresses, ships a new jump table. ~130ms
  patches.
- Works across native **and wasm** (via the wasm indirect-function-table + shared linear
  memory) — the *only* wasm-capable option, precisely because it never overwrites code.
- Limits: **tip crate only** (deps not patched), no struct-layout/type changes, statics/globals.
- Adopted by Bevy, Iced.

### Live++ (Stefan Reinalter / Molecular Matters)
- The gold standard for C/C++ live coding. Technique: **patch a `jmp` into the function
  prologue** (transparent, innermost). Reads PDB to recompile+relink single files.
- **Native only** — Windows / Xbox / PS5. **No wasm, no Linux/mac.**
- **Rust support: never shipped** (roadmap "under investigation" since 2022).
- In 2022 he tried to make Live++ support Rust ([internals thread](https://internals.rust-lang.org/t/trying-to-make-live-support-rust/16519)).
  His verdict: Rust is hot-reloadable, it just needs a few rustc/cargo flags — not a Live++
  rewrite. His blockers: (1) `/OPT:REF` hardcoded in rustc, (2) no `/hotpatch` entry padding,
  (3) cargo relocates `.o` files away from where the PDB points. He even **forked Clang & LLD**
  temporarily to emit missing PDB records (`S_OBJNAME`, `LF_BUILDINFO`). His framing: fork as a
  *bridge*, get the flags *upstreamed*, don't maintain a fork.
- Key quote: *"dragging the function pointer around does not undo previously executed code"* —
  the tool moves code; state is the developer's problem. And: the value of live coding is
  **state preservation**, not compile speed.

### hot-lib-reloader
- Native **dylib-swap** (not prologue-patch, not transparent). Mature-ish but **stale**
  (v0.8.2, 2025-08). Gotchas: crashes with `tracing`, breaks ECS TypeId assumptions, no generics
  (`#[no_mangle]`). Reloadable code must live behind a dylib boundary; state must live in the host.

### Dioxus (the framework)
- Production-ready framework, but its **mature** hot-reload is **RSX markup + assets only**
  (not arbitrary Rust logic). The Rust-logic hot-reload is subsecond (experimental). Requires
  writing UI in RSX + the `dx` CLI — **not usable as standalone tooling** for an arbitrary crate.

## Key decisions

1. **Don't fork rustc.** The one codegen knob we'd add (`-Zpatchable-function-entry`) already
   ships since 1.81. Everyone serious (subsecond/ThinLink, Live++) *drives* the compiler from
   outside; nobody forks it. Forking couples you to unstable internals + a multi-hour build +
   toolchain distribution + chasing every release.

2. **Native, not wasm.** wasm code is immutable; prologue patching is physically impossible
   there. `-Zpatchable-function-entry` is a *no-op* on wasm. For wasm you'd need the jump-table
   route (= reinvent subsecond). We target native, where the flag is real and code is writable.

3. **Prologue-patch, not jump-table.** We want *transparent, innermost* patching (edit any
   function, no annotations) — that's the prologue approach. The jump-table approach is
   intrusive (`subsecond::call` sites) and its only advantage (wasm) we've ruled out.

4. **LSP as watcher, not compiler.** rust-analyzer cannot codegen — but it's the perfect
   *change oracle and safety router*: which fn changed, is it body-only (patchable) or
   structural (rebuild), and is the edit even valid. This is what makes the engine *safe* and
   *minimal*. See ARCHITECTURE.md.

5. **Don't chase a maintained third-party hot-reloader — there isn't one** that's
   maintained + mature + fine-grained + native-Rust. subsecond (experimental), hot-lib-reloader
   (stale), Live++ (no Rust). Owning a small engine we understand beats depending on a fragile
   or absent one.

## The compiler flag (confirmed by source inspection)

From a stock rustc checkout (~mid-2026, beta 2026-05-31):

- `compiler/rustc_feature/src/unstable.rs`:
  `(unstable, patchable_function_entry, "1.81.0", Some(123115))` — unstable since 1.81, issue #123115.
- `compiler/rustc_codegen_llvm/src/attributes.rs`, `patchable_function_entry_attrs()`: reads
  `-Zpatchable-function-entry` (entry/prefix/section), emits LLVM `patchable-function-entry`
  attribute; also honors a per-fn `#[patchable_function_entry(...)]`.
- The Windows-only `-Zhotpatch` sugar (PR #134004: ≥2-byte first instruction + auto
  `functionpadmin`) was **not** in that checkout — the *general* primitive is what's present,
  which is what we want.

## Sources

- Subsecond: https://docs.rs/subsecond/latest/subsecond/ · https://dioxuslabs.com/blog/release-070/
- Live++: https://liveplusplus.tech/blog/ · https://blog.s-schoener.com/2024-12-16-liveplusplus-debug/ · https://molecular-matters.com/products_livepp.html
- Live++ trying Rust (internals): https://internals.rust-lang.org/t/trying-to-make-live-support-rust/16519
- rustc `-Zhotpatch` PR: https://github.com/rust-lang/rust/pull/134004
- `patchable-function-entry` RFC: https://github.com/rust-lang/rfcs/pull/3543
- hot-lib-reloader: https://docs.rs/hot-lib-reloader/latest/hot_lib_reloader/
- rustc source: https://github.com/rust-lang/rust (`compiler/rustc_codegen_llvm/src/attributes.rs`)
