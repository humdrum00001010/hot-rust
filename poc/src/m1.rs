//! M1: prove the heart of a native prologue-patch hot-reload engine.
//!
//! Build so every function starts with overwritable NOP padding:
//!   RUSTC_BOOTSTRAP=1 RUSTFLAGS="-Zpatchable-function-entry=16" cargo run --bin m1
//!
//! We then overwrite `target()`'s entry with a branch to `replacement()`, in the running
//! process. A *direct call* to `target()` returns the new value afterwards — the call site
//! is untouched. That is the Live++ property: transparent, innermost, in-place patching.
//!
//! Implements x86-64 and ARM64 branch encoders plus Windows/macOS code-memory writers.
//! Verified on x86_64-apple-darwin under Rosetta and native aarch64-apple-darwin, including
//! default `__TEXT` through a Frida-style remap-copy fallback. See ../ROADMAP.md (M1).

use std::hint::black_box;
use std::{fmt, io};

const PATCHABLE_ENTRY_BYTES: usize = 16;

#[inline(never)]
#[cfg_attr(
    all(
        target_os = "macos",
        target_arch = "aarch64",
        feature = "hot-segment-arm64"
    ),
    link_section = "__HOTRST,__text"
)]
extern "C" fn target() -> u32 {
    black_box(1)
}

#[inline(never)]
extern "C" fn replacement() -> u32 {
    black_box(2)
}

#[derive(Debug)]
enum PatchError {
    JumpOutOfRange {
        old_fn: usize,
        new_fn: usize,
        reach: &'static str,
    },
    #[cfg(target_arch = "aarch64")]
    MisalignedArm64Branch {
        old_fn: usize,
        new_fn: usize,
    },
    Protect(&'static str, io::Error),
    MissingPatchPadding {
        entry: Vec<u8>,
        needed: usize,
    },
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JumpOutOfRange {
                old_fn,
                new_fn,
                reach,
            } => write!(
                f,
                "replacement is out of range for this jump encoding ({old_fn:#x} -> {new_fn:#x}, reach {reach})"
            ),
            #[cfg(target_arch = "aarch64")]
            Self::MisalignedArm64Branch { old_fn, new_fn } => write!(
                f,
                "ARM64 branch target/source are not 4-byte aligned ({old_fn:#x} -> {new_fn:#x})"
            ),
            Self::Protect(op, source) => write!(f, "{op} failed: {source}"),
            Self::MissingPatchPadding { entry, needed } => write!(
                f,
                "function entry does not start with at least {needed} bytes of recognized patch padding: {entry:02x?}"
            ),
        }
    }
}

impl std::error::Error for PatchError {}

#[cfg(target_arch = "x86_64")]
fn encode_jump(old_fn: usize, new_fn: usize) -> Result<[u8; 5], PatchError> {
    let rel = (new_fn as isize).wrapping_sub(old_fn as isize + 5);
    if rel < i32::MIN as isize || rel > i32::MAX as isize {
        return Err(PatchError::JumpOutOfRange {
            old_fn,
            new_fn,
            reach: "+/-2GB",
        });
    }

    let mut bytes = [0_u8; 5];
    bytes[0] = 0xE9; // jmp rel32
    bytes[1..].copy_from_slice(&(rel as i32).to_le_bytes());
    Ok(bytes)
}

#[cfg(target_arch = "aarch64")]
fn encode_jump(old_fn: usize, new_fn: usize) -> Result<[u8; 4], PatchError> {
    let delta = (new_fn as isize).wrapping_sub(old_fn as isize);
    if delta % 4 != 0 {
        return Err(PatchError::MisalignedArm64Branch { old_fn, new_fn });
    }

    let imm26 = delta / 4;
    if !(-(1 << 25)..=(1 << 25) - 1).contains(&imm26) {
        return Err(PatchError::JumpOutOfRange {
            old_fn,
            new_fn,
            reach: "+/-128MB",
        });
    }

    // B imm26: 000101 + signed 26-bit immediate, scaled by 4 bytes.
    let instruction = 0x1400_0000_u32 | ((imm26 as u32) & 0x03ff_ffff);
    Ok(instruction.to_le_bytes())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("M1 currently has jump encoders for x86_64 and aarch64 only");

#[cfg(target_arch = "x86_64")]
fn has_patch_padding(entry: &[u8], needed: usize) -> bool {
    let mut offset = 0;
    while offset < needed {
        let Some(len) = x86_nop_len(&entry[offset..]) else {
            return false;
        };
        offset += len;
    }
    true
}

#[cfg(target_arch = "x86_64")]
fn x86_nop_len(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while matches!(bytes.get(i), Some(0x66 | 0x2e | 0x3e | 0x26 | 0x64 | 0x65)) {
        i += 1;
    }

    match bytes.get(i)? {
        0x90 => Some(i + 1),
        0x0f if bytes.get(i + 1) == Some(&0x1f) => {
            let modrm = *bytes.get(i + 2)?;
            let mode = modrm >> 6;
            let rm = modrm & 0b111;

            let mut len = i + 3;
            if mode != 0b11 && rm == 0b100 {
                len += 1; // SIB
            }

            len += match (mode, rm) {
                (0b00, 0b101) => 4,
                (0b01, _) => 1,
                (0b10, _) => 4,
                _ => 0,
            };

            (len <= bytes.len()).then_some(len)
        }
        _ => None,
    }
}

#[cfg(target_arch = "aarch64")]
fn has_patch_padding(entry: &[u8], needed: usize) -> bool {
    const ARM64_NOP: [u8; 4] = [0x1f, 0x20, 0x03, 0xd5];

    let needed_instructions = needed.div_ceil(4);
    entry
        .chunks_exact(4)
        .take(needed_instructions)
        .all(|instruction| instruction == ARM64_NOP)
}

/// Overwrite the entry NOP padding of `old_fn` with a branch to `new_fn`.
/// After the jump, the remaining padding bytes are dead code -- never executed.
unsafe fn patch(old_fn: usize, new_fn: usize) -> Result<(), PatchError> {
    let site = old_fn as *mut u8;
    let jump = encode_jump(old_fn, new_fn)?;
    let entry = core::slice::from_raw_parts(site, PATCHABLE_ENTRY_BYTES);
    if !has_patch_padding(entry, jump.len()) {
        return Err(PatchError::MissingPatchPadding {
            entry: entry.to_vec(),
            needed: jump.len(),
        });
    }

    println!("patch: {old_fn:#x} -> {new_fn:#x}, bytes {jump:02x?}");
    platform::write_code(site, &jump).map_err(|err| PatchError::Protect("code patch", err))
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), PatchError> {
    println!(
        "target() = {:#x}, replacement() = {:#x}",
        target as *const () as usize, replacement as *const () as usize
    );

    // Verify the flag took effect: the entry should be NOPs (0x90 / multi-byte 0f 1f ...).
    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, PATCHABLE_ENTRY_BYTES);
        println!("target() entry bytes (expect NOP padding): {:02x?}", entry);
    }
    unsafe {
        platform::validate_patch_site(target as *const () as usize)
            .map_err(|err| PatchError::Protect("patch site validation", err))?;
    }

    println!("before patch: target() = {}", black_box(target()));
    unsafe {
        patch(
            target as *const () as usize,
            replacement as *const () as usize,
        )?;
    }
    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, PATCHABLE_ENTRY_BYTES);
        println!("target() entry bytes after patch:          {:02x?}", entry);
    }
    println!(
        "after  patch: target() = {}   (replacement() itself = {})",
        black_box(target()),
        black_box(replacement())
    );

    assert_eq!(
        target(),
        2,
        "hot-patch failed: direct call did not redirect"
    );
    println!("OK: direct call to target() now runs replacement()'s body -- call site untouched.");
    Ok(())
}

#[cfg(target_os = "windows")]
mod platform {
    use std::io;

    const PAGE_EXECUTE_READWRITE: u32 = 0x40;

    // kernel32 -- declared directly, no external crate.
    extern "system" {
        fn VirtualProtect(addr: *mut u8, size: usize, new_prot: u32, old_prot: *mut u32) -> i32;
        fn FlushInstructionCache(process: isize, addr: *const u8, size: usize) -> i32;
        fn GetCurrentProcess() -> isize;
    }

    pub unsafe fn write_code(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        let mut old_prot = 0_u32;
        if VirtualProtect(
            site,
            super::PATCHABLE_ENTRY_BYTES,
            PAGE_EXECUTE_READWRITE,
            &mut old_prot,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        core::ptr::copy_nonoverlapping(bytes.as_ptr(), site, bytes.len());

        let mut restored_prot = 0_u32;
        if VirtualProtect(
            site,
            super::PATCHABLE_ENTRY_BYTES,
            old_prot,
            &mut restored_prot,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        if FlushInstructionCache(GetCurrentProcess(), site, super::PATCHABLE_ENTRY_BYTES) == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    pub unsafe fn validate_patch_site(_site: usize) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::io;

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const PROT_READ: i32 = 0x01;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const PROT_WRITE: i32 = 0x02;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const PROT_EXEC: i32 = 0x04;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_PROT_READ: i32 = 0x01;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_PROT_WRITE: i32 = 0x02;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_PROT_EXECUTE: i32 = 0x04;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_PROT_COPY: i32 = 0x10;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const _SC_PAGESIZE: i32 = 29;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const KERN_SUCCESS: i32 = 0;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const KERN_NO_SPACE: i32 = 3;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_FLAGS_FIXED: i32 = 0x0000;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_FLAGS_ANYWHERE: i32 = 0x0001;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_FLAGS_OVERWRITE: i32 = 0x4000;
    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    const VM_INHERIT_COPY: i32 = 1;
    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    const LC_SEGMENT_64: u32 = 0x19;

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    #[repr(C)]
    struct MachHeader64 {
        magic: u32,
        cputype: i32,
        cpusubtype: i32,
        filetype: u32,
        ncmds: u32,
        sizeofcmds: u32,
        flags: u32,
        reserved: u32,
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    #[repr(C)]
    struct LoadCommand {
        cmd: u32,
        cmdsize: u32,
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    #[repr(C)]
    struct SegmentCommand64 {
        cmd: u32,
        cmdsize: u32,
        segname: [u8; 16],
        vmaddr: u64,
        vmsize: u64,
        fileoff: u64,
        filesize: u64,
        maxprot: i32,
        initprot: i32,
        nsects: u32,
        flags: u32,
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_allocate(target: u32, address: *mut u64, size: u64, flags: i32) -> i32;
        fn mach_vm_deallocate(target: u32, address: u64, size: u64) -> i32;
        fn mach_vm_protect(
            target_task: u32,
            address: u64,
            size: u64,
            set_maximum: i32,
            new_protection: i32,
        ) -> i32;
        fn mach_vm_remap(
            target_task: u32,
            target_address: *mut u64,
            size: u64,
            mask: u64,
            flags: i32,
            src_task: u32,
            src_address: u64,
            copy: i32,
            cur_protection: *mut i32,
            max_protection: *mut i32,
            inheritance: i32,
        ) -> i32;
        fn mach_vm_write(target_task: u32, address: u64, data: usize, data_count: u32) -> i32;
        fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
        fn sysconf(name: i32) -> isize;
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    extern "C" {
        fn _dyld_get_image_header(image_index: u32) -> *const MachHeader64;
        fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
        fn pthread_jit_write_protect_np(enabled: i32);
        fn pthread_jit_write_protect_supported_np() -> i32;
    }

    #[cfg(all(target_arch = "aarch64", not(feature = "hot-segment-arm64")))]
    extern "C" {
        fn sys_dcache_flush(start: *mut c_void, len: usize);
    }

    #[cfg(target_arch = "aarch64")]
    extern "C" {
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    pub unsafe fn write_code(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        let (page_start, page_len) = page_span(site as usize, super::PATCHABLE_ENTRY_BYTES);

        if let Err(protect_err) = protect(
            page_start,
            page_len,
            PROT_READ | PROT_WRITE | PROT_EXEC,
            VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE | VM_PROT_COPY,
        ) {
            return match write_with_mach_vm(site, bytes, &protect_err) {
                Ok(()) => Ok(()),
                Err(write_err) => write_with_remapped_copy(site, bytes, &write_err),
            };
        }

        core::ptr::copy_nonoverlapping(bytes.as_ptr(), site, bytes.len());
        flush_instruction_cache(site, bytes.len());

        protect(
            page_start,
            page_len,
            PROT_READ | PROT_EXEC,
            VM_PROT_READ | VM_PROT_EXECUTE,
        )?;

        Ok(())
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    pub unsafe fn write_code(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        write_code_direct_for_hot_segment(site, bytes)
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    pub unsafe fn validate_patch_site(_site: usize) -> io::Result<()> {
        Ok(())
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    pub unsafe fn validate_patch_site(site: usize) -> io::Result<()> {
        if segment_has_initial_write(site) {
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "hot-segment-arm64 requires -Clink-arg=-Wl,-segprot,__HOTRST,rwx,rwx before target() can be called",
        ))
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    fn page_span(addr: usize, len: usize) -> (usize, usize) {
        let page_size = unsafe { sysconf(_SC_PAGESIZE) };
        let page_size = if page_size > 0 {
            page_size as usize
        } else {
            16 * 1024
        };
        let page_start = addr & !(page_size - 1);
        let page_end = (addr + len + page_size - 1) & !(page_size - 1);
        (page_start, page_end - page_start)
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    unsafe fn protect(
        page_start: usize,
        page_len: usize,
        mprotect_prot: i32,
        vm_prot: i32,
    ) -> io::Result<()> {
        if mprotect(page_start as *mut c_void, page_len, mprotect_prot) == 0 {
            return Ok(());
        }

        let mprotect_err = io::Error::last_os_error();
        let kr = mach_vm_protect(
            mach_task_self_,
            page_start as u64,
            page_len as u64,
            0,
            vm_prot,
        );
        if kr == 0 {
            return Ok(());
        }

        let max_kr = if vm_prot & VM_PROT_WRITE != 0 {
            mach_vm_protect(
                mach_task_self_,
                page_start as u64,
                page_len as u64,
                1,
                vm_prot,
            )
        } else {
            kr
        };
        if max_kr == 0 {
            let retry_kr = mach_vm_protect(
                mach_task_self_,
                page_start as u64,
                page_len as u64,
                0,
                vm_prot,
            );
            if retry_kr == 0 {
                return Ok(());
            }

            return Err(io::Error::new(
                mprotect_err.kind(),
                format!(
                    "mprotect failed with {mprotect_err}; mach_vm_protect current returned {kr}; max returned {max_kr}; retry returned {retry_kr}"
                ),
            ));
        }

        Err(io::Error::new(
            mprotect_err.kind(),
            format!(
                "mprotect failed with {mprotect_err}; mach_vm_protect current returned {kr}; max returned {max_kr}"
            ),
        ))
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    unsafe fn write_with_mach_vm(
        site: *mut u8,
        bytes: &[u8],
        protect_err: &io::Error,
    ) -> io::Result<()> {
        let kr = mach_vm_write(
            mach_task_self_,
            site as u64,
            bytes.as_ptr() as usize,
            bytes.len() as u32,
        );
        if kr != 0 {
            return Err(io::Error::new(
                protect_err.kind(),
                format!("{protect_err}; mach_vm_write returned {kr}"),
            ));
        }

        flush_instruction_cache(site, bytes.len());
        Ok(())
    }

    #[cfg(not(all(target_arch = "aarch64", feature = "hot-segment-arm64")))]
    unsafe fn write_with_remapped_copy(
        site: *mut u8,
        bytes: &[u8],
        prior_err: &io::Error,
    ) -> io::Result<()> {
        let (page_start, page_len) = page_span(site as usize, super::PATCHABLE_ENTRY_BYTES);
        let page_offset = (site as usize) - page_start;

        let mut scratch = 0_u64;
        let alloc_kr = mach_vm_allocate(
            mach_task_self_,
            &mut scratch,
            page_len as u64,
            VM_FLAGS_ANYWHERE,
        );
        if alloc_kr != KERN_SUCCESS {
            return Err(io::Error::new(
                prior_err.kind(),
                format!("{prior_err}; frida-style scratch allocation returned {alloc_kr}"),
            ));
        }

        core::ptr::copy_nonoverlapping(page_start as *const u8, scratch as *mut u8, page_len);
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (scratch as usize + page_offset) as *mut u8,
            bytes.len(),
        );
        flush_caches(scratch as *mut u8, page_len);

        let protect_scratch_kr = mach_vm_protect(
            mach_task_self_,
            scratch,
            page_len as u64,
            0,
            VM_PROT_READ | VM_PROT_EXECUTE,
        );
        if protect_scratch_kr != KERN_SUCCESS {
            mach_vm_deallocate(mach_task_self_, scratch, page_len as u64);
            return Err(io::Error::new(
                prior_err.kind(),
                format!("{prior_err}; frida-style scratch RX returned {protect_scratch_kr}"),
            ));
        }

        let mut target = page_start as u64;
        let mut cur = 0_i32;
        let mut max = 0_i32;
        let mut remap_kr = mach_vm_remap(
            mach_task_self_,
            &mut target,
            page_len as u64,
            0,
            VM_FLAGS_OVERWRITE | VM_FLAGS_FIXED,
            mach_task_self_,
            scratch,
            1,
            &mut cur,
            &mut max,
            VM_INHERIT_COPY,
        );

        if remap_kr == KERN_NO_SPACE {
            let preprotect_kr = mach_vm_protect(
                mach_task_self_,
                page_start as u64,
                page_len as u64,
                0,
                VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY,
            );
            if preprotect_kr == KERN_SUCCESS {
                target = page_start as u64;
                remap_kr = mach_vm_remap(
                    mach_task_self_,
                    &mut target,
                    page_len as u64,
                    0,
                    VM_FLAGS_OVERWRITE | VM_FLAGS_FIXED,
                    mach_task_self_,
                    scratch,
                    1,
                    &mut cur,
                    &mut max,
                    VM_INHERIT_COPY,
                );

                let _ = mach_vm_protect(
                    mach_task_self_,
                    page_start as u64,
                    page_len as u64,
                    0,
                    VM_PROT_READ | VM_PROT_EXECUTE,
                );
            }
        }

        mach_vm_deallocate(mach_task_self_, scratch, page_len as u64);

        if remap_kr != KERN_SUCCESS {
            return Err(io::Error::new(
                prior_err.kind(),
                format!("{prior_err}; frida-style remap copy returned {remap_kr}"),
            ));
        }

        flush_caches(page_start as *mut u8, page_len);
        println!("code patch: direct write failed ({prior_err}); frida-style remap copy succeeded");
        Ok(())
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    unsafe fn write_code_direct_for_hot_segment(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() != 4 || !(site as usize).is_multiple_of(4) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ARM64 hot segment writes require one aligned 4-byte instruction",
            ));
        }
        if !segment_has_initial_write(site as usize) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "hot-segment-arm64 requires the target function to be in a segment with initial write permission; pass -Clink-arg=-Wl,-segprot,__HOTRST,rwx,rwx",
            ));
        }

        let jit_supported = pthread_jit_write_protect_supported_np() != 0;
        if jit_supported {
            pthread_jit_write_protect_np(0);
        }

        let instruction = u32::from_le_bytes(bytes.try_into().expect("checked length"));
        core::ptr::write_volatile(site as *mut u32, instruction);
        flush_instruction_cache(site, bytes.len());

        if jit_supported {
            pthread_jit_write_protect_np(1);
        }

        Ok(())
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    unsafe fn segment_has_initial_write(addr: usize) -> bool {
        let header = _dyld_get_image_header(0);
        if header.is_null() {
            return false;
        }

        let header = &*header;
        let slide = _dyld_get_image_vmaddr_slide(0);
        let mut command = (header as *const MachHeader64).add(1).cast::<u8>();

        for _ in 0..header.ncmds {
            let load_command = &*(command as *const LoadCommand);
            if load_command.cmd == LC_SEGMENT_64 {
                let segment = &*(command as *const SegmentCommand64);
                let start = (segment.vmaddr as isize + slide) as usize;
                let end = start.saturating_add(segment.vmsize as usize);
                if (start..end).contains(&addr) {
                    let required = PROT_WRITE_FOR_HOT_SEGMENT | PROT_EXEC_FOR_HOT_SEGMENT;
                    return segment.initprot & required == required;
                }
            }

            command = command.add(load_command.cmdsize as usize);
        }

        false
    }

    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    const PROT_WRITE_FOR_HOT_SEGMENT: i32 = 0x02;
    #[cfg(all(target_arch = "aarch64", feature = "hot-segment-arm64"))]
    const PROT_EXEC_FOR_HOT_SEGMENT: i32 = 0x04;

    #[cfg(target_arch = "aarch64")]
    unsafe fn flush_instruction_cache(site: *mut u8, len: usize) {
        sys_icache_invalidate(site as *mut c_void, len);
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe fn flush_instruction_cache(_site: *mut u8, _len: usize) {}

    #[cfg(all(target_arch = "aarch64", not(feature = "hot-segment-arm64")))]
    unsafe fn flush_caches(site: *mut u8, len: usize) {
        sys_dcache_flush(site as *mut c_void, len);
        sys_icache_invalidate(site as *mut c_void, len);
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe fn flush_caches(_site: *mut u8, _len: usize) {}
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod platform {
    use std::io;

    pub unsafe fn write_code(_site: *mut u8, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "M1 currently implements code-memory writes for Windows and macOS only",
        ))
    }

    pub unsafe fn validate_patch_site(_site: usize) -> io::Result<()> {
        Ok(())
    }
}
