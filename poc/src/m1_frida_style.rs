//! Frida-style macOS code patch experiment for M1.
//!
//! This probes whether default Apple Silicon `__TEXT` can be patched without
//! temporarily making the original mapping RWX:
//!   1. remap the target page to a writable alias and write through that alias;
//!   2. if needed, copy the whole target page, patch the copy, and remap it over
//!      the original RX page.
//!
//! The default mode is a parent harness. The patch attempt happens in a child so
//! the ARM64 BRK page-plan path can trap without killing the cargo invocation.

use std::hint::black_box;
use std::io::{self, Write};
use std::process::{Command, Stdio};

const PATCHABLE_ENTRY_BYTES: usize = 16;

#[inline(never)]
extern "C" fn target() -> u32 {
    black_box(1)
}

#[inline(never)]
extern "C" fn replacement() -> u32 {
    black_box(2)
}

#[cfg(target_arch = "x86_64")]
fn encode_jump(old_fn: usize, new_fn: usize) -> Result<[u8; 5], String> {
    let rel = (new_fn as isize).wrapping_sub(old_fn as isize + 5);
    if rel < i32::MIN as isize || rel > i32::MAX as isize {
        return Err(format!(
            "replacement is out of range for x86_64 rel32 jump: {old_fn:#x} -> {new_fn:#x}"
        ));
    }

    let mut bytes = [0_u8; 5];
    bytes[0] = 0xe9;
    bytes[1..].copy_from_slice(&(rel as i32).to_le_bytes());
    Ok(bytes)
}

#[cfg(target_arch = "aarch64")]
fn encode_jump(old_fn: usize, new_fn: usize) -> Result<[u8; 4], String> {
    let delta = (new_fn as isize).wrapping_sub(old_fn as isize);
    if delta % 4 != 0 {
        return Err(format!(
            "ARM64 branch source/target not 4-byte aligned: {old_fn:#x} -> {new_fn:#x}"
        ));
    }

    let imm26 = delta / 4;
    if !(-(1 << 25)..=(1 << 25) - 1).contains(&imm26) {
        return Err(format!(
            "replacement is out of range for ARM64 B imm26: {old_fn:#x} -> {new_fn:#x}"
        ));
    }

    let instruction = 0x1400_0000_u32 | ((imm26 as u32) & 0x03ff_ffff);
    Ok(instruction.to_le_bytes())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("m1_frida_style currently has jump encoders for x86_64 and aarch64 only");

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
                len += 1;
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

fn main() {
    let result = if std::env::args().nth(1).as_deref() == Some("child") {
        child()
    } else {
        parent()
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn parent() -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new(std::env::current_exe()?)
        .arg("child")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    println!("child_status={}", output.status);

    if !output.status.success() {
        return Err(format!("child patch attempt failed with {}", output.status).into());
    }

    Ok(())
}

fn child() -> Result<(), Box<dyn std::error::Error>> {
    logln(format_args!("child_pid={}", std::process::id()));
    logln(format_args!(
        "target={:#x} replacement={:#x}",
        target as *const () as usize, replacement as *const () as usize
    ));

    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, PATCHABLE_ENTRY_BYTES);
        logln(format_args!("entry_before={:02x?}", entry));
    }
    logln(format_args!("before={}", black_box(target())));

    unsafe {
        patch_frida_style(
            target as *const () as usize,
            replacement as *const () as usize,
        )?;
    }

    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, PATCHABLE_ENTRY_BYTES);
        logln(format_args!("entry_after={:02x?}", entry));
    }
    logln(format_args!(
        "after={} replacement={}",
        black_box(target()),
        black_box(replacement())
    ));

    if target() != 2 {
        return Err("direct call still did not redirect to replacement()".into());
    }

    logln(format_args!("OK"));
    Ok(())
}

fn logln(args: std::fmt::Arguments<'_>) {
    println!("{args}");
    let _ = io::stdout().flush();
}

unsafe fn patch_frida_style(
    old_fn: usize,
    new_fn: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let site = old_fn as *mut u8;
    let jump = encode_jump(old_fn, new_fn)?;
    let entry = core::slice::from_raw_parts(site, PATCHABLE_ENTRY_BYTES);
    if !has_patch_padding(entry, jump.len()) {
        return Err(format!(
            "function entry does not start with recognized patch padding: {entry:02x?}"
        )
        .into());
    }

    logln(format_args!(
        "patch={old_fn:#x}->{new_fn:#x} bytes={jump:02x?}"
    ));

    platform::write_code_frida_style(site, &jump)?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::io;

    const KERN_SUCCESS: i32 = 0;
    const KERN_NO_SPACE: i32 = 3;

    const VM_FLAGS_FIXED: i32 = 0x0000;
    const VM_FLAGS_ANYWHERE: i32 = 0x0001;
    const VM_FLAGS_OVERWRITE: i32 = 0x4000;

    const VM_INHERIT_COPY: i32 = 1;
    const VM_INHERIT_NONE: i32 = 2;

    const VM_PROT_READ: i32 = 0x01;
    const VM_PROT_WRITE: i32 = 0x02;
    const VM_PROT_EXECUTE: i32 = 0x04;
    const VM_PROT_COPY: i32 = 0x10;

    const VM_PAGE_INFO_BASIC: i32 = 1;
    const VM_PAGE_QUERY_PAGE_CS_VALIDATED: i32 = 0x100;
    const VM_PAGE_QUERY_PAGE_CS_TAINTED: i32 = 0x200;
    const VM_PAGE_QUERY_PAGE_CS_NX: i32 = 0x400;

    const _SC_PAGESIZE: i32 = 29;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct VmPageInfoBasic {
        disposition: i32,
        ref_count: i32,
        object_id: u64,
        offset: u64,
        depth: i32,
        pad: i32,
    }

    extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_allocate(target: u32, address: *mut u64, size: u64, flags: i32) -> i32;
        fn mach_vm_deallocate(target: u32, address: u64, size: u64) -> i32;
        fn mach_vm_page_info(
            target_task: u32,
            address: u64,
            flavor: i32,
            info: *mut i32,
            info_count: *mut u32,
        ) -> i32;
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
        fn sysconf(name: i32) -> isize;
        fn sys_dcache_flush(start: *mut c_void, len: usize);
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    pub unsafe fn write_code_frida_style(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        let page_size = page_size();
        let page_start = (site as usize) & !(page_size - 1);
        let page_offset = (site as usize) - page_start;
        let page_len = page_size;

        log_page_info("target", page_start);
        match debugger_mapping_enforced(page_size) {
            Ok(enforced) => super::logln(format_args!("debugger_mapping_enforced={enforced}")),
            Err(err) => super::logln(format_args!("debugger_mapping_enforced_probe_error={err}")),
        }

        if try_writable_alias(page_start, page_offset, page_len, bytes)? {
            return Ok(());
        }

        try_remap_patched_copy(page_start, page_offset, page_len, bytes)
    }

    unsafe fn try_writable_alias(
        page_start: usize,
        page_offset: usize,
        page_len: usize,
        bytes: &[u8],
    ) -> io::Result<bool> {
        super::logln(format_args!("alias_remap: begin"));

        if std::env::var_os("HOT_RUST_TRY_PAGE_PLAN").is_some() {
            let plan_result = post_page_plan(page_start, page_len);
            match plan_result {
                Ok(raw) => super::logln(format_args!("alias_remap: page_plan_raw={raw:#x}")),
                Err(err) => super::logln(format_args!("alias_remap: page_plan_error={err}")),
            }
        } else {
            super::logln(format_args!(
                "alias_remap: page_plan_skipped=set HOT_RUST_TRY_PAGE_PLAN=1 to probe brk #1337"
            ));
        }

        let mut alias = 0_u64;
        let mut cur = 0_i32;
        let mut max = 0_i32;
        let remap_kr = mach_vm_remap(
            mach_task_self_,
            &mut alias,
            page_len as u64,
            0,
            VM_FLAGS_ANYWHERE,
            mach_task_self_,
            page_start as u64,
            0,
            &mut cur,
            &mut max,
            VM_INHERIT_NONE,
        );
        super::logln(format_args!(
            "alias_remap: mach_vm_remap kr={remap_kr} alias={alias:#x} cur={} max={}",
            prot(cur),
            prot(max)
        ));
        if remap_kr != KERN_SUCCESS {
            return Ok(false);
        }

        let protect_kr = mach_vm_protect(
            mach_task_self_,
            alias,
            page_len as u64,
            0,
            VM_PROT_READ | VM_PROT_WRITE,
        );
        super::logln(format_args!(
            "alias_remap: protect_alias_rw kr={protect_kr}"
        ));
        if protect_kr != KERN_SUCCESS {
            mach_vm_deallocate(mach_task_self_, alias, page_len as u64);
            return Ok(false);
        }

        let alias_site = (alias as usize + page_offset) as *mut u8;
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), alias_site, bytes.len());
        flush_caches(page_start, page_len);

        let target_bytes =
            core::slice::from_raw_parts((page_start + page_offset) as *const u8, bytes.len());
        super::logln(format_args!(
            "alias_remap: target_after_alias_write={target_bytes:02x?}"
        ));

        mach_vm_deallocate(mach_task_self_, alias, page_len as u64);
        Ok(target_bytes == bytes)
    }

    unsafe fn try_remap_patched_copy(
        page_start: usize,
        page_offset: usize,
        page_len: usize,
        bytes: &[u8],
    ) -> io::Result<()> {
        super::logln(format_args!("copy_remap: begin"));

        let mut scratch = 0_u64;
        let alloc_kr = mach_vm_allocate(
            mach_task_self_,
            &mut scratch,
            page_len as u64,
            VM_FLAGS_ANYWHERE,
        );
        super::logln(format_args!(
            "copy_remap: allocate_scratch kr={alloc_kr} scratch={scratch:#x}"
        ));
        if alloc_kr != KERN_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("scratch mach_vm_allocate returned {alloc_kr}"),
            ));
        }

        core::ptr::copy_nonoverlapping(page_start as *const u8, scratch as *mut u8, page_len);
        core::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (scratch as usize + page_offset) as *mut u8,
            bytes.len(),
        );
        flush_caches(scratch as usize, page_len);

        let protect_scratch_kr = mach_vm_protect(
            mach_task_self_,
            scratch,
            page_len as u64,
            0,
            VM_PROT_READ | VM_PROT_EXECUTE,
        );
        super::logln(format_args!(
            "copy_remap: protect_scratch_rx kr={protect_scratch_kr}"
        ));
        if protect_scratch_kr != KERN_SUCCESS {
            mach_vm_deallocate(mach_task_self_, scratch, page_len as u64);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("scratch mach_vm_protect RX returned {protect_scratch_kr}"),
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
        super::logln(format_args!(
            "copy_remap: remap_over_target kr={remap_kr} target={target:#x} cur={} max={}",
            prot(cur),
            prot(max)
        ));

        if remap_kr == KERN_NO_SPACE {
            let preprotect_kr = mach_vm_protect(
                mach_task_self_,
                page_start as u64,
                page_len as u64,
                0,
                VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY,
            );
            super::logln(format_args!(
                "copy_remap: no_space_preprotect_rw_copy kr={preprotect_kr}"
            ));

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
            super::logln(format_args!(
                "copy_remap: remap_retry kr={remap_kr} target={target:#x} cur={} max={}",
                prot(cur),
                prot(max)
            ));

            let restore_kr = mach_vm_protect(
                mach_task_self_,
                page_start as u64,
                page_len as u64,
                0,
                VM_PROT_READ | VM_PROT_EXECUTE,
            );
            super::logln(format_args!("copy_remap: restore_rx kr={restore_kr}"));
        }

        mach_vm_deallocate(mach_task_self_, scratch, page_len as u64);

        if remap_kr != KERN_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("copy remap over target returned {remap_kr}"),
            ));
        }

        flush_caches(page_start, page_len);
        log_page_info("target_after_copy_remap", page_start);
        Ok(())
    }

    unsafe fn debugger_mapping_enforced(page_size: usize) -> io::Result<bool> {
        let mut start = 0_u64;
        let alloc_kr = mach_vm_allocate(
            mach_task_self_,
            &mut start,
            page_size as u64,
            VM_FLAGS_ANYWHERE,
        );
        if alloc_kr != KERN_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("probe mach_vm_allocate returned {alloc_kr}"),
            ));
        }

        *(start as *mut u32) = 1337;
        let protect_kr = mach_vm_protect(
            mach_task_self_,
            start,
            page_size as u64,
            0,
            VM_PROT_READ | VM_PROT_EXECUTE,
        );
        if protect_kr != KERN_SUCCESS {
            mach_vm_deallocate(mach_task_self_, start, page_size as u64);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("probe mach_vm_protect RX returned {protect_kr}"),
            ));
        }

        let mut alias = 0_u64;
        let mut cur = 0_i32;
        let mut max = 0_i32;
        let remap_kr = mach_vm_remap(
            mach_task_self_,
            &mut alias,
            page_size as u64,
            0,
            VM_FLAGS_ANYWHERE,
            mach_task_self_,
            start,
            0,
            &mut cur,
            &mut max,
            VM_INHERIT_NONE,
        );
        super::logln(format_args!(
            "debugger_mapping_probe: remap kr={remap_kr} alias={alias:#x} cur={} max={}",
            prot(cur),
            prot(max)
        ));

        if alias != 0 {
            mach_vm_deallocate(mach_task_self_, alias, page_size as u64);
        }
        mach_vm_deallocate(mach_task_self_, start, page_size as u64);

        if remap_kr != KERN_SUCCESS {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("probe mach_vm_remap returned {remap_kr}"),
            ));
        }

        Ok(cur & (VM_PROT_READ | VM_PROT_EXECUTE) != VM_PROT_READ | VM_PROT_EXECUTE)
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn post_page_plan(page_start: usize, page_len: usize) -> io::Result<usize> {
        let page_size = page_size();
        let n_pages = page_len / page_size;
        let mut packet = Vec::with_capacity(4 + 8 + 4 + n_pages);

        packet.extend_from_slice(&1_u32.to_ne_bytes());
        packet.extend_from_slice(&(page_start as u64).to_ne_bytes());
        packet.extend_from_slice(&(n_pages as u32).to_ne_bytes());

        for i in 0..n_pages {
            let page = page_start + (i * page_size);
            let byte = if i % 2 == 0 {
                *((page + page_size - 1) as *const u8)
            } else {
                *(page as *const u8)
            };
            packet.push(byte);
        }

        let raw_result: usize;
        core::arch::asm!(
            "mov x1, #1337",
            "mov x2, #1337",
            "mov x3, #3",
            "brk #1337",
            inout("x0") 0_usize => raw_result,
            in("x4") packet.len(),
            in("x5") packet.as_ptr() as usize,
            out("x1") _,
            out("x2") _,
            out("x3") _,
            options(nostack)
        );

        Ok(raw_result)
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe fn post_page_plan(_page_start: usize, _page_len: usize) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Frida page-plan BRK is ARM64-specific",
        ))
    }

    unsafe fn log_page_info(label: &str, page_start: usize) {
        let mut info = VmPageInfoBasic::default();
        let mut count =
            (std::mem::size_of::<VmPageInfoBasic>() / std::mem::size_of::<i32>()) as u32;
        let kr = mach_vm_page_info(
            mach_task_self_,
            page_start as u64,
            VM_PAGE_INFO_BASIC,
            (&mut info as *mut VmPageInfoBasic).cast::<i32>(),
            &mut count,
        );
        super::logln(format_args!(
            "{label}_page_info: kr={kr} disposition={:#x} flags={} ref_count={} object_id={:#x} depth={}",
            info.disposition,
            page_flags(info.disposition),
            info.ref_count,
            info.object_id,
            info.depth
        ));
    }

    fn page_flags(disposition: i32) -> String {
        let mut flags = Vec::new();
        if disposition & VM_PAGE_QUERY_PAGE_CS_VALIDATED != 0 {
            flags.push("CS_VALIDATED");
        }
        if disposition & VM_PAGE_QUERY_PAGE_CS_TAINTED != 0 {
            flags.push("CS_TAINTED");
        }
        if disposition & VM_PAGE_QUERY_PAGE_CS_NX != 0 {
            flags.push("CS_NX");
        }
        if flags.is_empty() {
            "none".to_string()
        } else {
            flags.join("|")
        }
    }

    fn prot(value: i32) -> String {
        let mut pieces = Vec::new();
        if value & VM_PROT_READ != 0 {
            pieces.push("r");
        }
        if value & VM_PROT_WRITE != 0 {
            pieces.push("w");
        }
        if value & VM_PROT_EXECUTE != 0 {
            pieces.push("x");
        }
        if value & VM_PROT_COPY != 0 {
            pieces.push("copy");
        }
        if pieces.is_empty() {
            format!("{value:#x}")
        } else {
            format!("{value:#x}({})", pieces.join(""))
        }
    }

    fn page_size() -> usize {
        let page_size = unsafe { sysconf(_SC_PAGESIZE) };
        if page_size > 0 {
            page_size as usize
        } else {
            16 * 1024
        }
    }

    unsafe fn flush_caches(addr: usize, len: usize) {
        sys_dcache_flush(addr as *mut c_void, len);
        sys_icache_invalidate(addr as *mut c_void, len);
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::io;

    pub unsafe fn write_code_frida_style(_site: *mut u8, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "m1_frida_style is implemented for macOS only",
        ))
    }
}
