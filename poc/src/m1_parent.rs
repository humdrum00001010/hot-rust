//! Parent-process patch attempt for M1 on macOS.
//!
//! This tests whether a controlling parent process can patch a child process's default
//! executable text page. The child only reports addresses, waits, flushes its own instruction
//! cache, and calls `target()` again. All writes are attempted by the parent through Mach APIs.

use std::hint::black_box;
use std::io::{self, BufRead, BufReader, Write};
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
compile_error!("m1_parent currently has jump encoders for x86_64 and aarch64 only");

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

fn child() -> Result<(), Box<dyn std::error::Error>> {
    println!("child_pid={}", std::process::id());
    println!(
        "target={:#x} replacement={:#x}",
        target as *const () as usize, replacement as *const () as usize
    );

    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, PATCHABLE_ENTRY_BYTES);
        println!("entry_before={:02x?}", entry);
    }
    println!("before={}", black_box(target()));
    println!("READY");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    #[cfg(target_arch = "aarch64")]
    unsafe {
        platform::flush_instruction_cache(target as *const () as usize, PATCHABLE_ENTRY_BYTES);
    }

    unsafe {
        let entry = core::slice::from_raw_parts(target as *const u8, PATCHABLE_ENTRY_BYTES);
        println!("entry_after={:02x?}", entry);
    }
    println!("after={}", black_box(target()));
    Ok(())
}

fn parent() -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .arg("child")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let mut child_stdin = child.stdin.take().expect("child stdin was piped");
    let child_stdout = child.stdout.take().expect("child stdout was piped");
    let mut reader = BufReader::new(child_stdout);

    let mut child_pid = child.id() as i32;
    let mut target_addr = None;
    let mut replacement_addr = None;
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err("child exited before READY".into());
        }
        print!("child: {line}");

        if let Some(pid) = line.strip_prefix("child_pid=") {
            child_pid = pid.trim().parse()?;
        } else if let Some(addrs) = line.strip_prefix("target=") {
            let mut pieces = addrs.split_whitespace();
            target_addr = pieces.next().map(parse_hex);
            replacement_addr = pieces
                .next()
                .and_then(|piece| piece.strip_prefix("replacement="))
                .map(parse_hex);
        } else if line.trim() == "READY" {
            break;
        }
    }

    let old_fn = target_addr.transpose()?.ok_or("missing target address")?;
    let new_fn = replacement_addr
        .transpose()?
        .ok_or("missing replacement address")?;
    let jump = encode_jump(old_fn, new_fn)?;
    println!("parent: child_pid={child_pid} patch={old_fn:#x}->{new_fn:#x} bytes={jump:02x?}");

    let patch_result = unsafe {
        platform::debug_attach(child_pid)
            .and_then(|()| {
                let patch_result = platform::remote_patch(child_pid, old_fn, &jump);
                let detach_result = platform::debug_detach(child_pid);

                match (patch_result, detach_result) {
                    (Ok(()), Ok(())) => Ok(()),
                    (Err(patch_err), Ok(())) => Err(patch_err),
                    (Ok(()), Err(detach_err)) => Err(detach_err),
                    (Err(patch_err), Err(detach_err)) => Err(io::Error::new(
                        patch_err.kind(),
                        format!("patch failed with {patch_err}; detach failed with {detach_err}"),
                    )),
                }
            })
            .map_err(|err| io::Error::new(err.kind(), format!("debug remote patch failed: {err}")))
    };

    writeln!(child_stdin, "go")?;
    drop(child_stdin);

    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        print!("child: {line}");
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("child exited with {status}").into());
    }

    patch_result?;
    Ok(())
}

fn parse_hex(s: &str) -> Result<usize, std::num::ParseIntError> {
    usize::from_str_radix(s.trim().trim_start_matches("0x"), 16)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::io;

    const VM_PROT_READ: i32 = 0x01;
    const VM_PROT_WRITE: i32 = 0x02;
    const VM_PROT_EXECUTE: i32 = 0x04;
    const VM_PROT_COPY: i32 = 0x10;

    extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_protect(
            target_task: u32,
            address: u64,
            size: u64,
            set_maximum: i32,
            new_protection: i32,
        ) -> i32;
        fn mach_vm_write(target_task: u32, address: u64, data: usize, data_count: u32) -> i32;
        fn task_for_pid(target_tport: u32, pid: i32, task: *mut u32) -> i32;
        fn ptrace(request: i32, pid: i32, addr: *mut c_void, data: i32) -> i32;
    }

    #[cfg(target_arch = "aarch64")]
    extern "C" {
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    pub unsafe fn remote_patch(pid: i32, old_fn: usize, bytes: &[u8]) -> io::Result<()> {
        let mut task = 0_u32;
        let kr = task_for_pid(mach_task_self_, pid, &mut task);
        if kr != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("task_for_pid({pid}) returned {kr}"),
            ));
        }
        println!("parent: task_for_pid ok, task={task}");

        let page_size = 16 * 1024_usize;
        let page_start = old_fn & !(page_size - 1);
        let prot = VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE | VM_PROT_COPY;

        let current_kr = mach_vm_protect(task, page_start as u64, page_size as u64, 0, prot);
        let max_kr = mach_vm_protect(task, page_start as u64, page_size as u64, 1, prot);
        let retry_kr = mach_vm_protect(task, page_start as u64, page_size as u64, 0, prot);
        println!("parent: mach_vm_protect current={current_kr} max={max_kr} retry={retry_kr}");

        let write_kr = mach_vm_write(
            task,
            old_fn as u64,
            bytes.as_ptr() as usize,
            bytes.len() as u32,
        );
        if write_kr != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("mach_vm_write returned {write_kr}"),
            ));
        }
        println!("parent: mach_vm_write ok");

        Ok(())
    }

    pub unsafe fn debug_attach(pid: i32) -> io::Result<()> {
        const PT_ATTACHEXC: i32 = 14;

        if ptrace(PT_ATTACHEXC, pid, std::ptr::null_mut(), 0) != 0 {
            return Err(io::Error::last_os_error());
        }

        println!("parent: ptrace attach ok");
        Ok(())
    }

    pub unsafe fn debug_detach(pid: i32) -> io::Result<()> {
        const PT_DETACH: i32 = 11;

        if ptrace(PT_DETACH, pid, std::ptr::null_mut(), 0) != 0 {
            return Err(io::Error::last_os_error());
        }

        println!("parent: ptrace detach ok");
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    pub unsafe fn flush_instruction_cache(addr: usize, len: usize) {
        sys_icache_invalidate(addr as *mut c_void, len);
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::io;

    pub unsafe fn remote_patch(_pid: i32, _old_fn: usize, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "m1_parent remote patch test is implemented for macOS only",
        ))
    }

    pub unsafe fn debug_attach(_pid: i32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "m1_parent debug attach test is implemented for macOS only",
        ))
    }

    pub unsafe fn debug_detach(_pid: i32) -> io::Result<()> {
        Ok(())
    }
}
