//! M1: prove the heart of a native prologue-patch hot-reload engine.
//!
//! Build so every function starts with overwritable NOP padding:
//!   RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1
//!
//! We then overwrite `target()`'s entry with a `jmp` to `replacement()`, in the running
//! process. A *direct call* to `target()` returns the new value afterwards — the call site
//! is untouched. That is the Live++ property: transparent, innermost, in-place patching.
//!
//! STATUS: designed, not yet built/verified. Windows/x86-64 only. See ../ROADMAP.md (M1).

use std::hint::black_box;

#[inline(never)]
extern "C" fn target() -> u32 {
    black_box(1)
}

#[inline(never)]
extern "C" fn replacement() -> u32 {
    black_box(2)
}

// kernel32 — declared directly, no external crate.
extern "system" {
    fn VirtualProtect(addr: *mut u8, size: usize, new_prot: u32, old_prot: *mut u32) -> i32;
    fn FlushInstructionCache(process: isize, addr: *const u8, size: usize) -> i32;
    fn GetCurrentProcess() -> isize;
}
const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// Overwrite the entry NOP padding of `old_fn` with `jmp rel32 -> new_fn`.
/// Assumes |new_fn - old_fn| < 2GB (true within one image) so a 5-byte `E9` reaches.
/// After the jump, the remaining padding bytes are dead code — never executed.
unsafe fn patch(old_fn: usize, new_fn: usize) {
    let site = old_fn as *mut u8;
    let rel = (new_fn as isize - (old_fn as isize + 5)) as i32;

    let mut old_prot = 0u32;
    VirtualProtect(site, 16, PAGE_EXECUTE_READWRITE, &mut old_prot);
    *site = 0xE9; // jmp rel32
    core::ptr::copy_nonoverlapping(rel.to_le_bytes().as_ptr(), site.add(1), 4);
    let mut _back = 0u32;
    VirtualProtect(site, 16, old_prot, &mut _back);
    FlushInstructionCache(GetCurrentProcess(), site, 16);
}

fn main() {
    // Verify the flag took effect: the entry should be NOPs (0x90 / multi-byte 0f 1f ...).
    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, 16);
        println!("target() entry bytes (expect NOP padding): {:02x?}", entry);
    }

    println!("before patch: target() = {}", black_box(target()));
    unsafe {
        patch(target as usize, replacement as usize);
    }
    println!(
        "after  patch: target() = {}   (replacement() itself = {})",
        black_box(target()),
        black_box(replacement())
    );

    assert_eq!(target(), 2, "hot-patch failed: direct call did not redirect");
    println!("OK: direct call to target() now runs replacement()'s body — call site untouched.");
}
