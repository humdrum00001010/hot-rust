//! Target-side runtime injected by `hr`.
//!
//! This is intentionally tiny: if `HR_SOCKET` is present, a constructor starts a
//! Unix socket server inside the target process. `hr` sends a JSON patch command
//! naming an old symbol in the main executable and a new symbol in a patch dylib.
//! The runtime resolves both addresses and patches the old entry from inside the
//! process, which avoids the macOS parent-process write restrictions.

use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread;

const PATCHABLE_ENTRY_BYTES: usize = 16;

static MAIN_SYMBOL_CACHE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();

fn main_symbol_cache() -> &'static Mutex<HashMap<String, usize>> {
    MAIN_SYMBOL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[used]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
static HR_RUNTIME_INIT: extern "C" fn() = hr_runtime_init;

extern "C" fn hr_runtime_init() {
    let Some(socket) = std::env::var_os("HR_SOCKET") else {
        return;
    };
    let socket = PathBuf::from(socket);
    thread::spawn(move || {
        if let Err(err) = run_server(&socket) {
            eprintln!("hr-runtime: server failed: {err}");
        }
    });
}

fn run_server(socket: &Path) -> Result<(), Box<dyn Error>> {
    let _ = fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    eprintln!("hr-runtime: listening {}", socket.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(err) = handle_stream(stream) {
                        eprintln!("hr-runtime: command failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("hr-runtime: accept failed: {err}"),
        }
    }
    Ok(())
}

fn handle_stream(mut stream: UnixStream) -> Result<(), Box<dyn Error>> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let command: Value = serde_json::from_str(&line)?;
    let old_symbol = command
        .get("old_symbol")
        .and_then(Value::as_str)
        .ok_or("missing old_symbol")?;
    let new_symbol = command
        .get("new_symbol")
        .and_then(Value::as_str)
        .ok_or("missing new_symbol")?;
    let validate_only = command
        .get("validate_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let old_addr = resolve_main_symbol(old_symbol)?;
    let (new_addr, keepalive) = if let Some(object_path) =
        command.get("object_path").and_then(Value::as_str)
    {
        let object = unsafe { LoadedObject::open(Path::new(object_path), new_symbol)? };
        let addr = object.entry;
        (addr, LoadedPatch::Object(object))
    } else {
        let patch_dylib = command
            .get("patch_dylib")
            .and_then(Value::as_str)
            .ok_or("missing patch_dylib")?;
        let library = unsafe { Library::open(Path::new(patch_dylib))? };
        if let Some(stubs) = command.get("stubs").and_then(Value::as_array) {
            for stub in stubs {
                let stub_symbol = stub
                    .get("stub_symbol")
                    .and_then(Value::as_str)
                    .ok_or("stub entry missing stub_symbol")?;
                let old_stub_symbol = stub
                    .get("old_symbol")
                    .and_then(Value::as_str)
                    .ok_or("stub entry missing old_symbol")?;
                let stub_addr = unsafe { library.symbol(stub_symbol)? as usize };
                let old_stub_addr = resolve_main_symbol(old_stub_symbol)?;
                if validate_only {
                    unsafe {
                        validate_patch_to_external(stub_addr, old_stub_addr)?;
                    }
                    eprintln!(
                        "hr-runtime: stub validated stub={stub_symbol} at {stub_addr:#x} -> {old_stub_symbol} at {old_stub_addr:#x}"
                    );
                } else {
                    unsafe {
                        patch_to_external(stub_addr, old_stub_addr)?;
                    }
                    eprintln!(
                        "hr-runtime: stub patched stub={stub_symbol} at {stub_addr:#x} -> {old_stub_symbol} at {old_stub_addr:#x}"
                    );
                }
            }
        }
        let addr = unsafe { library.symbol(new_symbol)? as usize };
        (addr, LoadedPatch::Library(library))
    };

    if validate_only {
        unsafe {
            validate_patch_to_external(old_addr, new_addr)?;
        }
        eprintln!(
            "hr-runtime: patch validated old={old_addr:#x} new={new_addr:#x} symbol={old_symbol}"
        );
        writeln!(
            stream,
            "OK validate old={old_addr:#x} new={new_addr:#x} symbol={old_symbol}"
        )?;
        stream.flush()?;
        return Ok(());
    }

    unsafe {
        patch_to_external(old_addr, new_addr)?;
    }
    eprintln!("hr-runtime: patch applied old={old_addr:#x} new={new_addr:#x} symbol={old_symbol}");
    writeln!(
        stream,
        "OK old={old_addr:#x} new={new_addr:#x} symbol={old_symbol}"
    )?;
    stream.flush()?;
    std::mem::forget(keepalive);
    Ok(())
}

// Intentionally "dead" by field access: the enum owns loaded code resources so
// they stay alive after patching. Do not replace with a bare address.
#[allow(dead_code)]
enum LoadedPatch {
    Library(Library),
    Object(LoadedObject),
}

struct LoadedObject {
    entry: usize,
    // Intentionally retained as mmap ownership for object patches.
    #[allow(dead_code)]
    base: *mut u8,
    // Paired with `base`; kept for eventual unmap/debug accounting.
    #[allow(dead_code)]
    size: usize,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl LoadedObject {
    unsafe fn open(path: &Path, symbol: &str) -> Result<Self, Box<dyn Error>> {
        macho_object::load_arm64_object(path, symbol)
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl LoadedObject {
    unsafe fn open(_path: &Path, _symbol: &str) -> Result<Self, Box<dyn Error>> {
        Err("object patching currently implemented for macOS arm64 only".into())
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macho_object {
    use super::*;
    use std::collections::HashMap;

    const MH_OBJECT: u32 = 0x1;
    const N_UNDF: u8 = 0x0;
    const ARM64_RELOC_UNSIGNED: u8 = 0;
    const ARM64_RELOC_BRANCH26: u8 = 2;
    const ARM64_RELOC_PAGE21: u8 = 3;
    const ARM64_RELOC_PAGEOFF12: u8 = 4;
    const ARM64_RELOC_ADDEND: u8 = 10;

    pub unsafe fn load_arm64_object(
        path: &Path,
        symbol: &str,
    ) -> Result<LoadedObject, Box<dyn Error>> {
        let bytes = fs::read(path)?;
        let object = ObjectFile::parse(&bytes)?;
        platform::jit_write_protect(false);
        let mut image = match LoadedImage::allocate(&bytes, &object) {
            Ok(image) => image,
            Err(err) => {
                platform::jit_write_protect(true);
                return Err(err);
            }
        };
        if let Err(err) = image.apply_relocations(&object, &bytes) {
            platform::jit_write_protect(true);
            return Err(err);
        }
        platform::flush_icache(image.base, image.size);
        platform::jit_write_protect(true);

        let entry = image
            .find_symbol(&object, symbol)?
            .ok_or_else(|| format!("object symbol `{symbol}` not found in {}", path.display()))?;
        eprintln!(
            "hr-runtime: object loaded path={} base={:#x} size={} entry={:#x} relocations={} stubs={}",
            path.display(),
            image.base as usize,
            image.size,
            entry,
            image.applied_relocations,
            image.branch_stubs.len(),
        );
        Ok(LoadedObject {
            entry,
            base: image.base,
            size: image.size,
        })
    }

    struct ObjectFile {
        sections: Vec<ObjectSection>,
        symbols: Vec<ObjectSymbol>,
    }

    impl ObjectFile {
        fn parse(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
            let magic = read_u32(bytes, 0)?;
            if magic != MH_MAGIC_64 {
                return Err(format!("object has unsupported Mach-O magic {magic:#x}").into());
            }
            let filetype = read_u32(bytes, 12)?;
            if filetype != MH_OBJECT {
                return Err(format!("object has unsupported Mach-O filetype {filetype:#x}").into());
            }
            let ncmds = read_u32(bytes, 16)? as usize;
            let mut offset = 32usize;
            let mut symtab = None;
            let mut sections = Vec::new();
            for _ in 0..ncmds {
                let cmd = read_u32(bytes, offset)?;
                let cmdsize = read_u32(bytes, offset + 4)? as usize;
                if cmd == LC_SYMTAB {
                    symtab = Some(Symtab {
                        symoff: read_u32(bytes, offset + 8)? as usize,
                        nsyms: read_u32(bytes, offset + 12)? as usize,
                        stroff: read_u32(bytes, offset + 16)? as usize,
                        strsize: read_u32(bytes, offset + 20)? as usize,
                    });
                } else if cmd == LC_SEGMENT_64 {
                    let nsects = read_u32(bytes, offset + 64)? as usize;
                    let mut section_offset = offset + 72;
                    for index in 0..nsects {
                        sections.push(ObjectSection {
                            index: sections.len() + 1,
                            sectname: read_fixed_cstr(bytes, section_offset, 16)?,
                            segname: read_fixed_cstr(bytes, section_offset + 16, 16)?,
                            addr: read_u64(bytes, section_offset + 32)?,
                            size: read_u64(bytes, section_offset + 40)?,
                            offset: read_u32(bytes, section_offset + 48)? as usize,
                            align: read_u32(bytes, section_offset + 52)?,
                            reloff: read_u32(bytes, section_offset + 56)? as usize,
                            nreloc: read_u32(bytes, section_offset + 60)? as usize,
                        });
                        let _ = index;
                        section_offset = section_offset
                            .checked_add(80)
                            .ok_or("mach-o object section offset overflow")?;
                    }
                }
                offset = offset
                    .checked_add(cmdsize)
                    .ok_or("mach-o object load command offset overflow")?;
            }
            let symtab = symtab.ok_or("object has no LC_SYMTAB")?;
            let mut symbols = Vec::with_capacity(symtab.nsyms);
            for index in 0..symtab.nsyms {
                let nlist = symtab
                    .symoff
                    .checked_add(index * 16)
                    .ok_or("mach-o object symbol offset overflow")?;
                let strx = read_u32(bytes, nlist)? as usize;
                let n_type = read_u8(bytes, nlist + 4)?;
                let n_sect = read_u8(bytes, nlist + 5)?;
                let n_value = read_u64(bytes, nlist + 8)?;
                let name = if strx == 0 {
                    String::new()
                } else {
                    let name_offset = symtab
                        .stroff
                        .checked_add(strx)
                        .ok_or("mach-o object string offset overflow")?;
                    if name_offset >= symtab.stroff + symtab.strsize || name_offset >= bytes.len() {
                        String::new()
                    } else {
                        read_cstr(&bytes[name_offset..])?.to_string()
                    }
                };
                symbols.push(ObjectSymbol {
                    name,
                    n_type,
                    n_sect,
                    n_value,
                });
            }
            Ok(Self { sections, symbols })
        }
    }

    struct ObjectSection {
        index: usize,
        sectname: String,
        segname: String,
        addr: u64,
        size: u64,
        offset: usize,
        align: u32,
        reloff: usize,
        nreloc: usize,
    }

    impl ObjectSection {
        fn is_loadable(&self) -> bool {
            matches!(self.segname.as_str(), "__TEXT" | "__DATA")
                && self.sectname != "__eh_frame"
                && self.size > 0
        }
    }

    struct ObjectSymbol {
        name: String,
        n_type: u8,
        n_sect: u8,
        n_value: u64,
    }

    struct LoadedImage {
        base: *mut u8,
        size: usize,
        stub_offset: usize,
        applied_relocations: usize,
        branch_stubs: HashMap<usize, usize>,
    }

    impl LoadedImage {
        unsafe fn allocate(bytes: &[u8], object: &ObjectFile) -> Result<Self, Box<dyn Error>> {
            let mut cursor = 0usize;
            let mut section_offsets = Vec::with_capacity(object.sections.len());
            let branch_stub_capacity = branch_relocation_count(object, bytes)?
                .checked_mul(16)
                .ok_or("branch stub capacity overflow")?;
            for section in &object.sections {
                if section.is_loadable() {
                    cursor = align_to(cursor, 1usize << section.align.min(20))?;
                    section_offsets.push(Some(cursor));
                    cursor = cursor
                        .checked_add(section.size as usize)
                        .ok_or("loaded object size overflow")?;
                } else {
                    section_offsets.push(None);
                }
            }
            cursor = align_to(cursor, 4)?;
            let stub_offset = cursor;
            cursor = cursor
                .checked_add(branch_stub_capacity)
                .ok_or("loaded object stub size overflow")?;
            let size = align_to(cursor.max(1), 16_384)?;
            let base = platform::alloc_rwx(size)?;
            std::ptr::write_bytes(base, 0, size);

            for (section, loaded_offset) in object.sections.iter().zip(section_offsets) {
                let Some(loaded_offset) = loaded_offset else {
                    continue;
                };
                let src = bytes
                    .get(section.offset..section.offset + section.size as usize)
                    .ok_or_else(|| {
                        format!(
                            "section {}.{} payload outside object",
                            section.segname, section.sectname
                        )
                    })?;
                std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(loaded_offset), src.len());
            }

            let image = Self {
                base,
                size,
                stub_offset,
                applied_relocations: 0,
                branch_stubs: HashMap::new(),
            };
            Ok(image)
        }

        fn section_addr(&self, object: &ObjectFile, section_index: usize) -> Option<usize> {
            object.sections.get(section_index.checked_sub(1)?)?;
            let offset = loaded_section_offset(object, section_index)?;
            Some(self.base as usize + offset)
        }

        fn find_symbol(
            &self,
            object: &ObjectFile,
            symbol: &str,
        ) -> Result<Option<usize>, Box<dyn Error>> {
            let spellings = object_symbol_spellings(symbol);
            for candidate in spellings {
                for object_symbol in &object.symbols {
                    if object_symbol.name == candidate {
                        return self.symbol_addr(object, object_symbol).map(Some);
                    }
                }
            }
            Ok(None)
        }

        fn symbol_addr(
            &self,
            object: &ObjectFile,
            symbol: &ObjectSymbol,
        ) -> Result<usize, Box<dyn Error>> {
            if symbol.n_type & N_TYPE == N_SECT {
                let section_index = symbol.n_sect as usize;
                let section = object
                    .sections
                    .get(section_index.checked_sub(1).ok_or("zero section index")?)
                    .ok_or("symbol references missing section")?;
                let section_addr = self
                    .section_addr(object, section_index)
                    .ok_or("symbol references unloaded section")?;
                let delta = symbol
                    .n_value
                    .checked_sub(section.addr)
                    .ok_or("symbol value before section address")?;
                return Ok(section_addr + delta as usize);
            }
            if symbol.n_type & N_TYPE == N_UNDF {
                return resolve_process_symbol_macho_name(&symbol.name);
            }
            Err(format!(
                "unsupported object symbol type {:#x} for {}",
                symbol.n_type, symbol.name
            )
            .into())
        }

        unsafe fn apply_relocations(
            &mut self,
            object: &ObjectFile,
            bytes: &[u8],
        ) -> Result<(), Box<dyn Error>> {
            for section in &object.sections {
                if !section.is_loadable() {
                    continue;
                }
                let Some(section_base) = self.section_addr(object, section.index) else {
                    continue;
                };
                let mut pending_addend = 0i64;
                for index in 0..section.nreloc {
                    let offset = section
                        .reloff
                        .checked_add(index * 8)
                        .ok_or("relocation offset overflow")?;
                    let relocation = Relocation::parse_from_object(bytes, offset)?;
                    if relocation.r_type == ARM64_RELOC_ADDEND {
                        pending_addend = sign_extend_24(relocation.r_symbolnum as u32);
                        continue;
                    }
                    let addend = pending_addend;
                    pending_addend = 0;
                    let place = section_base
                        .checked_add(relocation.r_address as usize)
                        .ok_or("relocation place overflow")?;
                    let target = self.relocation_target(object, &relocation, addend)?;
                    match relocation.r_type {
                        ARM64_RELOC_UNSIGNED => apply_unsigned(place, relocation.r_length, target)?,
                        ARM64_RELOC_BRANCH26 => self.apply_branch26(place, target)?,
                        ARM64_RELOC_PAGE21 => apply_page21(place, target)?,
                        ARM64_RELOC_PAGEOFF12 => apply_pageoff12(place, target)?,
                        other => {
                            return Err(format!(
                                "unsupported relocation type {other} in {}.{} at {:#x}",
                                section.segname, section.sectname, relocation.r_address
                            )
                            .into())
                        }
                    }
                    self.applied_relocations += 1;
                }
            }
            Ok(())
        }

        fn relocation_target(
            &self,
            object: &ObjectFile,
            relocation: &Relocation,
            addend: i64,
        ) -> Result<usize, Box<dyn Error>> {
            let base = if relocation.r_extern {
                let symbol = object
                    .symbols
                    .get(relocation.r_symbolnum)
                    .ok_or("relocation references missing symbol")?;
                self.symbol_addr(object, symbol)?
            } else {
                self.section_addr(object, relocation.r_symbolnum)
                    .ok_or("relocation references unloaded section")?
            };
            Ok((base as i64 + addend) as usize)
        }

        unsafe fn apply_branch26(
            &mut self,
            place: usize,
            target: usize,
        ) -> Result<(), Box<dyn Error>> {
            let branch_target = if branch26_fits(place, target) {
                target
            } else {
                self.branch_stub(target)?
            };
            let delta = (branch_target as isize)
                .checked_sub(place as isize)
                .ok_or("branch delta overflow")?;
            if delta % 4 != 0 {
                return Err("unaligned ARM64 branch target".into());
            }
            let imm26 = (delta / 4) as i64;
            if !(-(1 << 25)..(1 << 25)).contains(&imm26) {
                return Err(format!(
                    "ARM64 branch target still out of range: {place:#x} -> {branch_target:#x}"
                )
                .into());
            }
            let word = read_u32_raw(place as *const u8);
            write_u32_raw(
                place as *mut u8,
                (word & 0xfc00_0000) | ((imm26 as u32) & 0x03ff_ffff),
            );
            Ok(())
        }

        unsafe fn branch_stub(&mut self, target: usize) -> Result<usize, Box<dyn Error>> {
            if let Some(stub) = self.branch_stubs.get(&target) {
                return Ok(*stub);
            }
            let offset = self
                .stub_offset
                .checked_add(self.branch_stubs.len() * 16)
                .ok_or("branch stub offset overflow")?;
            if offset + 16 > self.size {
                return Err("branch stub capacity exhausted".into());
            }
            let stub = self.base.add(offset);
            write_u32_raw(stub, 0x5800_0050); // ldr x16, #8
            write_u32_raw(stub.add(4), 0xd61f_0200); // br x16
            std::ptr::copy_nonoverlapping(
                &(target as u64).to_le_bytes() as *const u8,
                stub.add(8),
                8,
            );
            let stub_addr = stub as usize;
            self.branch_stubs.insert(target, stub_addr);
            Ok(stub_addr)
        }
    }

    struct Relocation {
        r_address: i32,
        r_symbolnum: usize,
        r_length: u8,
        r_extern: bool,
        r_type: u8,
    }

    impl Relocation {
        fn parse_from_object(bytes: &[u8], offset: usize) -> Result<Self, Box<dyn Error>> {
            let r_address = read_i32(bytes, offset)?;
            if r_address < 0 {
                return Err("scattered Mach-O relocations are not supported".into());
            }
            let r_info = read_u32(bytes, offset + 4)?;
            Ok(Self {
                r_address,
                r_symbolnum: (r_info & 0x00ff_ffff) as usize,
                r_length: ((r_info >> 25) & 0x3) as u8,
                r_extern: ((r_info >> 27) & 0x1) != 0,
                r_type: ((r_info >> 28) & 0xf) as u8,
            })
        }
    }

    unsafe fn apply_unsigned(
        place: usize,
        length: u8,
        target: usize,
    ) -> Result<(), Box<dyn Error>> {
        match length {
            3 => {
                std::ptr::copy_nonoverlapping(
                    &(target as u64).to_le_bytes() as *const u8,
                    place as *mut u8,
                    8,
                );
                Ok(())
            }
            other => Err(format!("unsupported UNSIGNED relocation length {other}").into()),
        }
    }

    unsafe fn apply_page21(place: usize, target: usize) -> Result<(), Box<dyn Error>> {
        let place_page = place & !0xfff;
        let target_page = target & !0xfff;
        let delta_pages = ((target_page as isize) - (place_page as isize)) >> 12;
        if !(-(1 << 20)..(1 << 20)).contains(&(delta_pages as i64)) {
            return Err(format!("ADRP target out of range: {place:#x} -> {target:#x}").into());
        }
        let imm = (delta_pages as i64 as u32) & 0x1f_ffff;
        let immlo = imm & 0x3;
        let immhi = (imm >> 2) & 0x7_ffff;
        let word = read_u32_raw(place as *const u8);
        write_u32_raw(
            place as *mut u8,
            (word & !((0x3 << 29) | (0x7_ffff << 5))) | (immlo << 29) | (immhi << 5),
        );
        Ok(())
    }

    unsafe fn apply_pageoff12(place: usize, target: usize) -> Result<(), Box<dyn Error>> {
        let pageoff = (target & 0xfff) as u32;
        let word = read_u32_raw(place as *const u8);
        if word & 0xffc0_0000 != 0x9100_0000 {
            return Err(
                format!("unsupported PAGEOFF12 instruction {word:#x} at {place:#x}").into(),
            );
        }
        write_u32_raw(place as *mut u8, (word & !(0xfff << 10)) | (pageoff << 10));
        Ok(())
    }

    unsafe fn read_u32_raw(ptr: *const u8) -> u32 {
        u32::from_le_bytes(std::ptr::read_unaligned(ptr as *const [u8; 4]))
    }

    unsafe fn write_u32_raw(ptr: *mut u8, value: u32) {
        std::ptr::write_unaligned(ptr as *mut [u8; 4], value.to_le_bytes());
    }

    fn branch26_fits(place: usize, target: usize) -> bool {
        let delta = target as isize - place as isize;
        delta % 4 == 0 && (-(1 << 27)..(1 << 27)).contains(&delta)
    }

    fn sign_extend_24(value: u32) -> i64 {
        let value = value & 0x00ff_ffff;
        if value & 0x0080_0000 != 0 {
            (value | 0xff00_0000) as i32 as i64
        } else {
            value as i64
        }
    }

    fn branch_relocation_count(object: &ObjectFile, bytes: &[u8]) -> Result<usize, Box<dyn Error>> {
        let mut count = 0usize;
        for section in &object.sections {
            if !section.is_loadable() {
                continue;
            }
            for index in 0..section.nreloc {
                let relocation = Relocation::parse_from_object(bytes, section.reloff + index * 8)?;
                if relocation.r_type == ARM64_RELOC_BRANCH26 {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    fn loaded_section_offset(object: &ObjectFile, section_index: usize) -> Option<usize> {
        let mut cursor = 0usize;
        for section in &object.sections {
            let loaded = if section.is_loadable() {
                cursor = align_to(cursor, 1usize << section.align.min(20)).ok()?;
                let value = Some(cursor);
                cursor = cursor.checked_add(section.size as usize)?;
                value
            } else {
                None
            };
            if section.index == section_index {
                return loaded;
            }
        }
        None
    }

    fn object_symbol_spellings(symbol: &str) -> Vec<String> {
        let mut spellings = vec![symbol.to_string()];
        if !symbol.starts_with('_') {
            spellings.push(format!("_{symbol}"));
        } else {
            spellings.push(format!("_{symbol}"));
            spellings.push(symbol.trim_start_matches('_').to_string());
        }
        spellings.sort();
        spellings.dedup();
        spellings
    }

    fn align_to(value: usize, align: usize) -> Result<usize, Box<dyn Error>> {
        if align == 0 || !align.is_power_of_two() {
            return Err(format!("invalid alignment {align}").into());
        }
        Ok((value + align - 1) & !(align - 1))
    }
}

struct Library {
    handle: *mut c_void,
}

impl Library {
    unsafe fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        let path = CString::new(path.as_os_str().as_encoded_bytes())?;
        let handle = dlopen(path.as_ptr(), RTLD_NOW | RTLD_LOCAL);
        if handle.is_null() {
            return Err(dl_error().into());
        }
        Ok(Self { handle })
    }

    unsafe fn symbol(&self, name: &str) -> Result<*mut c_void, Box<dyn Error>> {
        let name = CString::new(name)?;
        let symbol = dlsym(self.handle, name.as_ptr());
        if symbol.is_null() {
            return Err(dl_error().into());
        }
        Ok(symbol)
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.handle);
        }
    }
}

fn dl_error() -> String {
    unsafe {
        let err = dlerror();
        if err.is_null() {
            "unknown dlerror".to_string()
        } else {
            CStr::from_ptr(err).to_string_lossy().into_owned()
        }
    }
}

fn resolve_main_symbol(symbol: &str) -> Result<usize, Box<dyn Error>> {
    if let Some(addr) = main_symbol_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(symbol).copied())
    {
        return Ok(addr);
    }

    let addr = resolve_main_symbol_uncached(symbol)?;
    if let Ok(mut cache) = main_symbol_cache().lock() {
        cache.insert(symbol.to_string(), addr);
    }
    Ok(addr)
}

fn resolve_main_symbol_uncached(symbol: &str) -> Result<usize, Box<dyn Error>> {
    unsafe {
        if let Ok(symbol) = CString::new(symbol) {
            let addr = dlsym(RTLD_DEFAULT, symbol.as_ptr());
            if !addr.is_null() {
                return Ok(addr as usize);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        resolve_main_symbol_macho(symbol)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Err(format!("symbol {symbol} not found with dlsym").into())
    }
}

#[cfg(target_os = "macos")]
fn resolve_main_symbol_macho(symbol: &str) -> Result<usize, Box<dyn Error>> {
    let exe = current_exe_from_dyld()?;
    let bytes = fs::read(&exe)?;
    let want = format!("_{symbol}");
    let slide = dyld_slide_for_image(&exe)?;

    let header = MachHeader64::parse(&bytes)?;
    let mut offset = 32usize;
    let mut symtab = None;
    let mut sections = Vec::new();
    for _ in 0..header.ncmds {
        let cmd = read_u32(&bytes, offset)?;
        let cmdsize = read_u32(&bytes, offset + 4)? as usize;
        if cmd == LC_SYMTAB {
            symtab = Some(Symtab {
                symoff: read_u32(&bytes, offset + 8)? as usize,
                nsyms: read_u32(&bytes, offset + 12)? as usize,
                stroff: read_u32(&bytes, offset + 16)? as usize,
                strsize: read_u32(&bytes, offset + 20)? as usize,
            });
        } else if cmd == LC_SEGMENT_64 {
            let nsects = read_u32(&bytes, offset + 64)? as usize;
            let mut section_offset = offset + 72;
            for _ in 0..nsects {
                sections.push(SectionInfo {
                    sectname: read_fixed_cstr(&bytes, section_offset, 16)?,
                    segname: read_fixed_cstr(&bytes, section_offset + 16, 16)?,
                });
                section_offset = section_offset
                    .checked_add(80)
                    .ok_or("mach-o section offset overflow")?;
            }
        }
        offset = offset
            .checked_add(cmdsize)
            .ok_or("mach-o load command offset overflow")?;
    }

    let symtab = symtab.ok_or("main executable has no LC_SYMTAB")?;
    for index in 0..symtab.nsyms {
        let nlist = symtab
            .symoff
            .checked_add(index * 16)
            .ok_or("mach-o symbol offset overflow")?;
        let strx = read_u32(&bytes, nlist)? as usize;
        let n_type = read_u8(&bytes, nlist + 4)?;
        let n_sect = read_u8(&bytes, nlist + 5)?;
        let value = read_u64(&bytes, nlist + 8)?;
        if strx == 0 || value == 0 {
            continue;
        }
        if n_type & N_STAB != 0 || n_type & N_TYPE != N_SECT {
            continue;
        }
        if !is_text_section(n_sect, &sections) {
            continue;
        }
        let name_offset = symtab
            .stroff
            .checked_add(strx)
            .ok_or("mach-o string offset overflow")?;
        if name_offset >= symtab.stroff + symtab.strsize || name_offset >= bytes.len() {
            continue;
        }
        let name = read_cstr(&bytes[name_offset..])?;
        if name == want {
            return Ok((value as i64 + slide) as usize);
        }
    }

    Err(format!("symbol {symbol} not found in {}", exe.display()).into())
}

#[cfg(target_os = "macos")]
fn resolve_process_symbol_macho_name(macho_name: &str) -> Result<usize, Box<dyn Error>> {
    unsafe {
        if let Some(stripped) = macho_name.strip_prefix('_') {
            if let Ok(name) = CString::new(stripped) {
                let addr = dlsym(RTLD_DEFAULT, name.as_ptr());
                if !addr.is_null() {
                    return Ok(addr as usize);
                }
            }
        }
        if let Ok(name) = CString::new(macho_name) {
            let addr = dlsym(RTLD_DEFAULT, name.as_ptr());
            if !addr.is_null() {
                return Ok(addr as usize);
            }
        }
    }

    let exe = current_exe_from_dyld()?;
    let bytes = fs::read(&exe)?;
    let slide = dyld_slide_for_image(&exe)?;
    let header = MachHeader64::parse(&bytes)?;
    let mut offset = 32usize;
    let mut symtab = None;
    for _ in 0..header.ncmds {
        let cmd = read_u32(&bytes, offset)?;
        let cmdsize = read_u32(&bytes, offset + 4)? as usize;
        if cmd == LC_SYMTAB {
            symtab = Some(Symtab {
                symoff: read_u32(&bytes, offset + 8)? as usize,
                nsyms: read_u32(&bytes, offset + 12)? as usize,
                stroff: read_u32(&bytes, offset + 16)? as usize,
                strsize: read_u32(&bytes, offset + 20)? as usize,
            });
        }
        offset = offset
            .checked_add(cmdsize)
            .ok_or("mach-o load command offset overflow")?;
    }

    let symtab = symtab.ok_or("main executable has no LC_SYMTAB")?;
    for index in 0..symtab.nsyms {
        let nlist = symtab
            .symoff
            .checked_add(index * 16)
            .ok_or("mach-o symbol offset overflow")?;
        let strx = read_u32(&bytes, nlist)? as usize;
        let n_type = read_u8(&bytes, nlist + 4)?;
        let value = read_u64(&bytes, nlist + 8)?;
        if strx == 0 || value == 0 {
            continue;
        }
        if n_type & N_STAB != 0 || n_type & N_TYPE != N_SECT {
            continue;
        }
        let name_offset = symtab
            .stroff
            .checked_add(strx)
            .ok_or("mach-o string offset overflow")?;
        if name_offset >= symtab.stroff + symtab.strsize || name_offset >= bytes.len() {
            continue;
        }
        let name = read_cstr(&bytes[name_offset..])?;
        if name == macho_name {
            return Ok((value as i64 + slide) as usize);
        }
    }

    Err(format!("process symbol {macho_name} not found in {}", exe.display()).into())
}

#[cfg(target_os = "macos")]
fn dyld_slide_for_image(exe: &Path) -> Result<i64, Box<dyn Error>> {
    let exe = exe.canonicalize()?;
    unsafe {
        for index in 0.._dyld_image_count() {
            let name = _dyld_get_image_name(index);
            if name.is_null() {
                continue;
            }
            let image = PathBuf::from(CStr::from_ptr(name).to_string_lossy().into_owned());
            let Ok(image) = image.canonicalize() else {
                continue;
            };
            if image == exe {
                return Ok(_dyld_get_image_vmaddr_slide(index) as i64);
            }
        }
    }
    Err(format!("could not find dyld image for {}", exe.display()).into())
}

#[cfg(target_os = "macos")]
fn current_exe_from_dyld() -> Result<PathBuf, Box<dyn Error>> {
    unsafe {
        let mut size = 0u32;
        let _ = _NSGetExecutablePath(std::ptr::null_mut(), &mut size);
        let mut buf = vec![0i8; size as usize + 1];
        let rc = _NSGetExecutablePath(buf.as_mut_ptr(), &mut size);
        if rc != 0 {
            return Err("_NSGetExecutablePath failed".into());
        }
        let raw = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        Ok(PathBuf::from(raw).canonicalize()?)
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MachHeader64 {
    ncmds: u32,
}

#[cfg(target_os = "macos")]
impl MachHeader64 {
    fn parse(bytes: &[u8]) -> Result<Self, Box<dyn Error>> {
        let magic = read_u32(bytes, 0)?;
        if magic != MH_MAGIC_64 {
            return Err(format!("unsupported mach-o magic {magic:#x}").into());
        }
        Ok(Self {
            ncmds: read_u32(bytes, 16)?,
        })
    }
}

#[cfg(target_os = "macos")]
const N_STAB: u8 = 0xe0;
#[cfg(target_os = "macos")]
const N_TYPE: u8 = 0x0e;
#[cfg(target_os = "macos")]
const N_SECT: u8 = 0x0e;

#[cfg(target_os = "macos")]
struct Symtab {
    symoff: usize,
    nsyms: usize,
    stroff: usize,
    strsize: usize,
}

#[cfg(target_os = "macos")]
struct SectionInfo {
    sectname: String,
    segname: String,
}

#[cfg(target_os = "macos")]
fn is_text_section(n_sect: u8, sections: &[SectionInfo]) -> bool {
    let Some(section) = n_sect
        .checked_sub(1)
        .and_then(|index| sections.get(index as usize))
    else {
        return false;
    };
    section.segname == "__TEXT" && section.sectname == "__text"
}

#[cfg(target_os = "macos")]
fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, Box<dyn Error>> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| "unexpected EOF reading u8".into())
}

#[cfg(target_os = "macos")]
fn read_cstr(bytes: &[u8]) -> Result<&str, Box<dyn Error>> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("unterminated mach-o string")?;
    Ok(std::str::from_utf8(&bytes[..end])?)
}

#[cfg(target_os = "macos")]
fn read_fixed_cstr(bytes: &[u8], offset: usize, len: usize) -> Result<String, Box<dyn Error>> {
    let raw = bytes
        .get(offset..offset + len)
        .ok_or("unexpected EOF reading fixed cstr")?;
    let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
    Ok(std::str::from_utf8(&raw[..end])?.to_string())
}

#[cfg(target_os = "macos")]
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Box<dyn Error>> {
    let bytes = bytes
        .get(offset..offset + 4)
        .ok_or("unexpected EOF reading u32")?;
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

#[cfg(target_os = "macos")]
fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, Box<dyn Error>> {
    let bytes = bytes
        .get(offset..offset + 4)
        .ok_or("unexpected EOF reading i32")?;
    Ok(i32::from_le_bytes(bytes.try_into()?))
}

#[cfg(target_os = "macos")]
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, Box<dyn Error>> {
    let bytes = bytes
        .get(offset..offset + 8)
        .ok_or("unexpected EOF reading u64")?;
    Ok(u64::from_le_bytes(bytes.try_into()?))
}

#[derive(Debug)]
enum PatchError {
    #[cfg(target_arch = "aarch64")]
    MisalignedArm64Branch {
        old_fn: usize,
    },
    MissingPatchPadding {
        entry: Vec<u8>,
        needed: usize,
    },
    Protect(std::io::Error),
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(target_arch = "aarch64")]
            Self::MisalignedArm64Branch { old_fn } => {
                write!(f, "ARM64 branch source is not 4-byte aligned ({old_fn:#x})")
            }
            Self::MissingPatchPadding { entry, needed } => write!(
                f,
                "function entry does not start with at least {needed} bytes of patch padding: {entry:02x?}"
            ),
            Self::Protect(err) => write!(f, "code patch failed: {err}"),
        }
    }
}

impl Error for PatchError {}

struct PatchBytes {
    bytes: Vec<u8>,
}

unsafe fn validate_patch_to_external(old_fn: usize, new_fn: usize) -> Result<(), PatchError> {
    validated_patch_to_external(old_fn, new_fn).map(|_| ())
}

unsafe fn patch_to_external(old_fn: usize, new_fn: usize) -> Result<(), PatchError> {
    let (site, patch) = validated_patch_to_external(old_fn, new_fn)?;
    platform::write_code(site, &patch.bytes).map_err(PatchError::Protect)
}

unsafe fn validated_patch_to_external(
    old_fn: usize,
    new_fn: usize,
) -> Result<(*mut u8, PatchBytes), PatchError> {
    let patch = encode_absolute_jump(old_fn, new_fn)?;
    if patch.bytes.len() > PATCHABLE_ENTRY_BYTES {
        return Err(PatchError::MissingPatchPadding {
            entry: Vec::new(),
            needed: patch.bytes.len(),
        });
    }

    let site = old_fn as *mut u8;
    let entry = core::slice::from_raw_parts(site, PATCHABLE_ENTRY_BYTES);
    if !has_patchable_entry(entry, patch.bytes.len()) {
        return Err(PatchError::MissingPatchPadding {
            entry: entry.to_vec(),
            needed: patch.bytes.len(),
        });
    }

    Ok((site, patch))
}

#[cfg(target_arch = "aarch64")]
fn encode_absolute_jump(old_fn: usize, new_fn: usize) -> Result<PatchBytes, PatchError> {
    if old_fn % 4 != 0 {
        return Err(PatchError::MisalignedArm64Branch { old_fn });
    }
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&0x5800_0050_u32.to_le_bytes());
    bytes.extend_from_slice(&0xd61f_0200_u32.to_le_bytes());
    bytes.extend_from_slice(&(new_fn as u64).to_le_bytes());
    Ok(PatchBytes { bytes })
}

#[cfg(target_arch = "x86_64")]
fn encode_absolute_jump(_old_fn: usize, new_fn: usize) -> Result<PatchBytes, PatchError> {
    let mut bytes = Vec::with_capacity(14);
    bytes.extend_from_slice(&[0xff, 0x25, 0x00, 0x00, 0x00, 0x00]);
    bytes.extend_from_slice(&(new_fn as u64).to_le_bytes());
    Ok(PatchBytes { bytes })
}

fn has_patchable_entry(bytes: &[u8], needed: usize) -> bool {
    if bytes.len() < needed {
        return false;
    }

    #[cfg(target_arch = "aarch64")]
    {
        if needed % 4 != 0 {
            return false;
        }
        let is_nop_padding = bytes[..needed]
            .chunks_exact(4)
            .all(|chunk| chunk == [0x1f, 0x20, 0x03, 0xd5]);
        let is_existing_hot_jump = needed == 16
            && bytes.get(0..4) == Some(&0x5800_0050_u32.to_le_bytes())
            && bytes.get(4..8) == Some(&0xd61f_0200_u32.to_le_bytes());
        return is_nop_padding || is_existing_hot_jump;
    }

    #[cfg(target_arch = "x86_64")]
    {
        let is_nop_padding = bytes[..needed]
            .iter()
            .all(|byte| *byte == 0x90 || *byte == 0xcc);
        let is_existing_hot_jump =
            needed == 14 && bytes.get(0..6) == Some(&[0xff, 0x25, 0x00, 0x00, 0x00, 0x00]);
        return is_nop_padding || is_existing_hot_jump;
    }

    #[allow(unreachable_code)]
    false
}

#[cfg(target_os = "macos")]
mod platform {
    #![allow(non_camel_case_types)]

    use super::*;
    use std::io;

    pub unsafe fn alloc_rwx(size: usize) -> io::Result<*mut u8> {
        let ptr = mmap(
            std::ptr::null_mut(),
            size,
            VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        );
        if ptr as isize != -1 {
            Ok(ptr as *mut u8)
        } else {
            let plain_error = io::Error::last_os_error();
            let ptr = mmap(
                std::ptr::null_mut(),
                size,
                VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE,
                MAP_PRIVATE | MAP_ANON | MAP_JIT,
                -1,
                0,
            );
            if ptr as isize != -1 {
                Ok(ptr as *mut u8)
            } else {
                Err(io::Error::other(format!(
                    "mmap RWX failed: {plain_error}; mmap MAP_JIT failed: {}",
                    io::Error::last_os_error()
                )))
            }
        }
    }

    pub unsafe fn jit_write_protect(enabled: bool) {
        pthread_jit_write_protect_np(if enabled { 1 } else { 0 });
    }

    pub unsafe fn flush_icache(start: *mut u8, len: usize) {
        sys_icache_invalidate(start as *mut c_void, len);
    }

    pub unsafe fn write_code(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        write_code_remap_copy(site, bytes)
    }

    unsafe fn write_code_remap_copy(site: *mut u8, bytes: &[u8]) -> io::Result<()> {
        let page_size = page_size()?;
        let site_addr = site as usize;
        let page_start = site_addr & !(page_size - 1);
        let page_offset = site_addr - page_start;
        if page_offset + bytes.len() > page_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "patch crosses a page boundary",
            ));
        }

        let mut copy_addr: mach_vm_address_t = 0;
        let alloc_kr = mach_vm_allocate(
            mach_task_self(),
            &mut copy_addr,
            page_size as mach_vm_size_t,
            VM_FLAGS_ANYWHERE,
        );
        if alloc_kr != KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "mach_vm_allocate returned {alloc_kr}"
            )));
        }

        std::ptr::copy_nonoverlapping(page_start as *const u8, copy_addr as *mut u8, page_size);
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (copy_addr as usize + page_offset) as *mut u8,
            bytes.len(),
        );
        let protect_kr = mach_vm_protect(
            mach_task_self(),
            copy_addr,
            page_size as mach_vm_size_t,
            0,
            VM_PROT_READ | VM_PROT_EXECUTE,
        );
        if protect_kr != KERN_SUCCESS {
            let _ = mach_vm_deallocate(mach_task_self(), copy_addr, page_size as mach_vm_size_t);
            return Err(io::Error::other(format!(
                "mach_vm_protect copy RX returned {protect_kr}"
            )));
        }

        let mut target_addr = page_start as mach_vm_address_t;
        let mut cur_protection: vm_prot_t = 0;
        let mut max_protection: vm_prot_t = 0;
        let remap_kr = mach_vm_remap(
            mach_task_self(),
            &mut target_addr,
            page_size as mach_vm_size_t,
            0,
            VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE,
            mach_task_self(),
            copy_addr,
            1,
            &mut cur_protection,
            &mut max_protection,
            VM_INHERIT_COPY,
        );
        let _ = mach_vm_deallocate(mach_task_self(), copy_addr, page_size as mach_vm_size_t);
        if remap_kr != KERN_SUCCESS {
            return Err(io::Error::other(format!(
                "mach_vm_remap returned {remap_kr}"
            )));
        }
        sys_icache_invalidate(site as *mut c_void, bytes.len());
        Ok(())
    }

    fn page_size() -> io::Result<usize> {
        let value = unsafe { sysconf(_SC_PAGESIZE) };
        if value <= 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value as usize)
        }
    }

    type kern_return_t = c_int;
    type mach_port_t = u32;
    type mach_vm_address_t = u64;
    type mach_vm_size_t = u64;
    type vm_prot_t = c_int;
    type vm_inherit_t = u32;
    type boolean_t = c_int;

    const KERN_SUCCESS: kern_return_t = 0;
    const VM_FLAGS_ANYWHERE: c_int = 1;
    const VM_FLAGS_FIXED: c_int = 0;
    const VM_FLAGS_OVERWRITE: c_int = 0x4000;
    const VM_PROT_READ: vm_prot_t = 1;
    const VM_PROT_WRITE: vm_prot_t = 2;
    const VM_PROT_EXECUTE: vm_prot_t = 4;
    const VM_INHERIT_COPY: vm_inherit_t = 1;
    const MAP_PRIVATE: c_int = 0x0002;
    const MAP_JIT: c_int = 0x0800;
    const MAP_ANON: c_int = 0x1000;
    const _SC_PAGESIZE: c_int = 29;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: isize,
        ) -> *mut c_void;
        fn pthread_jit_write_protect_np(enabled: c_int);
        fn mach_task_self() -> mach_port_t;
        fn mach_vm_allocate(
            target: mach_port_t,
            address: *mut mach_vm_address_t,
            size: mach_vm_size_t,
            flags: c_int,
        ) -> kern_return_t;
        fn mach_vm_deallocate(
            target: mach_port_t,
            address: mach_vm_address_t,
            size: mach_vm_size_t,
        ) -> kern_return_t;
        fn mach_vm_protect(
            target: mach_port_t,
            address: mach_vm_address_t,
            size: mach_vm_size_t,
            set_maximum: boolean_t,
            new_protection: vm_prot_t,
        ) -> kern_return_t;
        fn mach_vm_remap(
            target_task: mach_port_t,
            target_address: *mut mach_vm_address_t,
            size: mach_vm_size_t,
            mask: mach_vm_address_t,
            flags: c_int,
            src_task: mach_port_t,
            src_address: mach_vm_address_t,
            copy: boolean_t,
            cur_protection: *mut vm_prot_t,
            max_protection: *mut vm_prot_t,
            inheritance: vm_inherit_t,
        ) -> kern_return_t;
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
        fn sysconf(name: c_int) -> isize;
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::io;

    pub unsafe fn write_code(_site: *mut u8, _bytes: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hr_runtime patching currently implemented for macOS",
        ))
    }
}

const RTLD_NOW: c_int = 0x2;
const RTLD_LOCAL: c_int = 0x4;
const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;

#[cfg(target_os = "macos")]
const MH_MAGIC_64: u32 = 0xfeedfacf;
#[cfg(target_os = "macos")]
const LC_SYMTAB: u32 = 0x2;
#[cfg(target_os = "macos")]
const LC_SEGMENT_64: u32 = 0x19;

extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

#[cfg(target_os = "macos")]
extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(image_index: u32) -> *const c_char;
    fn _dyld_get_image_vmaddr_slide(image_index: u32) -> isize;
    fn _NSGetExecutablePath(buf: *mut c_char, bufsize: *mut u32) -> c_int;
}
