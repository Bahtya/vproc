//! Virtual execve — load and execute ELF binaries inside coroutines.

use std::alloc::{alloc, Layout};
use std::collections::{BTreeSet, HashMap};
use std::ffi::c_int;
use std::sync::Arc;
use std::os::raw::c_void;
use std::sync::Mutex;

use crate::coroutine::VPid;
use crate::elf;
use crate::loader;

const ELF_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MiB for loaded binaries

struct BinaryCacheEntry {
    main_addr: usize,
    /// Saved writable segment contents (addr, saved_bytes) for state reset.
    /// Wrapped in Arc so cache lookups don't clone the entire snapshot.
    saved_writable: Arc<Vec<(usize, Vec<u8>)>>,
}

/// Cache of loaded binary metadata per canonicalized path.
/// After the first dlopen + _start run, we save main()'s address and a snapshot
/// of all writable segments. Subsequent calls restore writable state so the
/// binary's globals are fresh, then call main() directly — skipping _start
/// entirely.
static BINARY_CACHE: std::sync::LazyLock<Mutex<HashMap<String, BinaryCacheEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Result of a virtual_execve operation.
pub struct VirtualExec {
    pub vpid: VPid,
}

/// Load and execute a static PIE ELF binary inside a new coroutine.
///
/// For dynamic binaries, use `virtual_execve_dynamic` instead.
pub fn virtual_execve_static(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
) -> Result<VirtualExec, String> {
    // Read ELF file
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read {}: {}", path, e))?;

    // Parse and validate
    let hdr = elf::parse_header(&data)?;
    if hdr.e_machine != elf::EM_AARCH64 {
        return Err("not aarch64".into());
    }

    // Check for interpreter (dynamic binary)
    let phdrs = elf::program_headers(&data, &hdr)?;
    let interp = elf::interpreter_path(&data, &phdrs)?;
    if interp.is_some() {
        return Err(
            "binary is dynamically linked — use virtual_execve_dynamic()".into(),
        );
    }

    // Load into memory
    let image = loader::load_pie(&data)?;

    // Build C strings (leaked — must survive the coroutine lifetime)
    let argv_c = argv
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect::<Vec<_>>();
    let envp_c = envp
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect::<Vec<_>>();

    // Build auxiliary vector
    let auxv = loader::build_auxv(&image, 0);

    // Allocate stack
    let stack_layout = Layout::from_size_align(ELF_STACK_SIZE, 16).map_err(|e| e.to_string())?;
    let stack_base = unsafe { alloc(stack_layout) };
    if stack_base.is_null() {
        return Err("stack allocation failed".into());
    }

    // Spawn ELF coroutine
    let vpid = crate::executor::EXECUTOR.with(|e| unsafe {
        (&mut *e.get()).spawn_elf(
            image.entry,
            stack_base,
            ELF_STACK_SIZE,
            argv_c.len(),
            argv_c,
            envp_c,
            auxv,
        )
    });

    Ok(VirtualExec { vpid })
}

/// Load and execute a dynamically-linked binary using dlopen.
///
/// The binary is loaded as a shared object via the dynamic linker.
/// Its `main` symbol is found and called inside a coroutine.
pub fn virtual_execve_dynamic(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
) -> Result<VirtualExec, String> {
    // Open the binary as a shared object
    let c_path = std::ffi::CString::new(path).map_err(|e| e.to_string())?;
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW) };
    if handle.is_null() {
        let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) }
            .to_string_lossy()
            .into_owned();
        return Err(format!("dlopen({}): {}", path, err));
    }

    // Find main() symbol
    let main_sym = unsafe { libc::dlsym(handle, b"main\0".as_ptr() as *const u8) };
    if main_sym.is_null() {
        unsafe { libc::dlclose(handle) };
        return Err(format!("dlsym(main) failed in {}", path));
    }

    // Build C strings
    let argv_c: Vec<*const u8> = argv
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect();
    let _envp_c: Vec<*const u8> = envp
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect();

    // Spawn a regular coroutine that calls main()
    let main_ptr = main_sym;
    let argc = argv_c.len() as c_int;
    let argv_ptr = argv_c.as_ptr() as *const *const i8;

    let vpid = crate::spawn(Box::new(move || {
        let main_fn: extern "C" fn(c_int, *const *const i8, *const *const i8) -> c_int =
            unsafe { std::mem::transmute(main_ptr) };
        let _exit_code = main_fn(argc, argv_ptr, std::ptr::null());
        // TODO: propagate exit code to virtual_waitpid
    }));

    Ok(VirtualExec { vpid })
}

/// Load and execute a dynamically-linked binary via dlopen + entry point jump.
///
/// Unlike `virtual_execve_dynamic` (which requires `dlsym("main")`),
/// this approach works with any PIE executable:
/// 1. dlopen loads the binary + all DT_NEEDED dependencies
/// 2. Find the loaded base address via dl_iterate_phdr
/// 3. Calculate entry = base + e_entry from the ELF header
/// 4. Spawn an ELF coroutine that jumps to the entry point
pub fn virtual_execve_via_entry(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
) -> Result<VirtualExec, String> {
    let real_path = std::fs::canonicalize(path)
        .map_err(|e| format!("cannot canonicalize {}: {}", path, e))?;
    let real_path_str = real_path.to_str().ok_or("invalid path")?.to_string();

    // Build C strings (leaked — must survive coroutine lifetime)
    let argv_c: Vec<*const u8> = argv
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect();
    let envp_c: Vec<*const u8> = envp
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect();
    let argc = argv_c.len();

    // Check if we already have main() cached for this binary
    let current_pid = unsafe { (*crate::executor::get_global_executor()).current };
    let cached = {
        let lock = BINARY_CACHE.lock().unwrap();
        lock.get(&real_path_str).map(|e| (e.main_addr, Arc::clone(&e.saved_writable)))
    };

    if let Some((main_addr, saved_writable)) = cached {
        // Binary already initialized — restore writable segments, call main() directly
        restore_writable_segments(&saved_writable);
        let vpid = spawn_main_coroutine(main_addr, argc, argv_c, envp_c);
        if let Some(pid) = current_pid {
            crate::vfd::fork_fd_table(pid, vpid);
        }
        return Ok(VirtualExec { vpid });
    }

    // First time: need to dlopen and run _start
    let data = std::fs::read(&real_path)
        .map_err(|e| format!("cannot read {}: {}", path, e))?;
    let hdr = elf::parse_header(&data)?;
    let e_entry = hdr.e_entry as usize;
    let phdrs = elf::program_headers(&data, &hdr)?;

    let c_path = std::ffi::CString::new(real_path_str.clone()).map_err(|e| e.to_string())?;
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) }
            .to_string_lossy()
            .into_owned();
        return Err(format!("dlopen({}): {}", path, err));
    }

    let base = find_loaded_base(path).ok_or_else(|| format!(
        "dlopen({}) succeeded but dl_iterate_phdr cannot find it", path
    ))?;

    let entry_addr = base + e_entry;

    // Extract main() address from _start instructions before running it
    let extracted = unsafe { extract_main_addr(entry_addr) };

    clear_init_arrays(base, &phdrs);
    hook_libc_exit();
    hook_libc_execve();
    patch_got_for_loaded_binary(base, &phdrs);

    // Save writable segment state AFTER clear_init_arrays + GOT patch but
    // BEFORE _start runs. The snapshot has zeroed init_arrays and patched GOT,
    // which is exactly what subsequent calls need restored before calling main().
    if let Some(main_addr) = extracted {
        let saved = save_writable_segments(base, &phdrs);
        BINARY_CACHE.lock().unwrap().insert(real_path_str.clone(), BinaryCacheEntry {
            main_addr,
            saved_writable: Arc::new(saved),
        });
    }

    let image = loader::LoadedImage {
        base,
        total_size: 0,
        entry: entry_addr,
        phdr_addr: base + (hdr.e_phoff as usize),
        phnum: hdr.e_phnum,
        phentsize: hdr.e_phentsize,
        interp_path: None,
    };
    let auxv = loader::build_auxv(&image, 0);

    let stack_layout = Layout::from_size_align(ELF_STACK_SIZE, 16)
        .map_err(|e| e.to_string())?;
    let stack_base = unsafe { alloc(stack_layout) };
    if stack_base.is_null() {
        return Err("stack allocation failed".into());
    }

    let vpid = unsafe {
        let ex = &mut *crate::executor::get_global_executor();
        // Pin executor to global pointer so it survives TLS reinitialization
        // when __libc_init runs inside the dlopen'd binary's _start.
        crate::executor::set_global_executor(ex as *mut _);
        ex.spawn_elf(
            entry_addr,
            stack_base,
            ELF_STACK_SIZE,
            argc,
            argv_c,
            envp_c,
            auxv,
        )
    };

    // Inherit fd table from current coroutine (Linux execve preserves fds)
    if let Some(pid) = current_pid {
        crate::vfd::fork_fd_table(pid, vpid);
    }

    Ok(VirtualExec { vpid })
}

/// Find the base address of a loaded shared object by pathname.
/// Resolves symlinks first since dl_iterate_phdr reports the real path.
fn find_loaded_base(path: &str) -> Option<usize> {
    let real_path = std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| path.to_string());
    let path_c = std::ffi::CString::new(real_path).ok()?;
    let result = std::cell::Cell::new(None::<usize>);

    // Store path in a static for the callback to access
    unsafe {
        SEARCH_PATH = path_c.as_ptr();
        libc::dl_iterate_phdr(
            Some(dl_iterate_callback),
            &result as *const _ as *mut c_void,
        );
    }

    result.get()
}

// Static used to pass the search path to the dl_iterate_phdr callback.
static mut SEARCH_PATH: *const std::os::raw::c_char = std::ptr::null();

unsafe extern "C" fn dl_iterate_callback(
    info: *mut libc::dl_phdr_info,
    _size: usize,
    data: *mut c_void,
) -> c_int {
    let info = &*info;
    let result = &*(data as *const std::cell::Cell<Option<usize>>);

    if info.dlpi_name.is_null() {
        return 0;
    }
    let name = std::ffi::CStr::from_ptr(info.dlpi_name);
    let search = std::ffi::CStr::from_ptr(SEARCH_PATH);

    if name == search {
        result.set(Some(info.dlpi_addr as usize));
        return 1; // stop iterating
    }
    0
}

/// Clear DT_INIT_ARRAY and DT_INIT entries in the loaded binary's memory.
/// This prevents double constructor execution when _start runs after dlopen
/// has already called them.
fn clear_init_arrays(base: usize, phdrs: &[elf::Phdr]) {
    // Find PT_DYNAMIC
    let dyn_phdr = match phdrs.iter().find(|p| p.p_type == elf::PT_DYNAMIC) {
        Some(p) => p,
        None => return,
    };

    let dyn_addr = base + (dyn_phdr.p_vaddr as usize);
    let dyn_size = dyn_phdr.p_memsz as usize / std::mem::size_of::<elf::Dyn>();

    // Parse dynamic entries and zero out init-related ones
    for i in 0..dyn_size {
        let dyn_ptr = (dyn_addr + i * std::mem::size_of::<elf::Dyn>()) as *mut elf::Dyn;
        let tag: i64 = unsafe { std::ptr::read_unaligned(std::ptr::addr_of_mut!((*dyn_ptr).d_tag)) }.into();
        match tag {
            elf::DT_INIT_ARRAY | elf::DT_INIT => {
                unsafe { std::ptr::write_unaligned(std::ptr::addr_of_mut!((*dyn_ptr).d_val), 0) };
            }
            _ => {}
        }
    }
}

/// Save contents of all writable PT_LOAD segments of the loaded binary.
/// Called after dlopen + GOT patching, before _start runs, so the snapshot
/// captures the correct initial state.
fn save_writable_segments(base: usize, phdrs: &[elf::Phdr]) -> Vec<(usize, Vec<u8>)> {
    let mut segments = Vec::new();
    for phdr in phdrs {
        if phdr.p_type != elf::PT_LOAD {
            continue;
        }
        if phdr.p_flags & elf::PF_W == 0 {
            continue;
        }
        let addr = base + (phdr.p_vaddr as usize);
        let size = phdr.p_memsz as usize;
        if size == 0 {
            continue;
        }
        let slice = unsafe { std::slice::from_raw_parts(addr as *const u8, size) };
        segments.push((addr, slice.to_vec()));
    }
    segments
}

/// Restore writable segment contents from a saved snapshot.
/// Re-mprotects pages writable, writes data, restores permissions.
fn restore_writable_segments(segments: &[(usize, Vec<u8>)]) {
    for &(addr, ref data) in segments {
        let size = data.len();
        if size == 0 {
            continue;
        }
        // Page-align
        let page_start = addr & !0xfff;
        let page_end = (addr + size + 0xfff) & !0xfff;
        let page_size = page_end - page_start;
        unsafe {
            if libc::mprotect(
                page_start as *mut c_void,
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
            ) != 0 {
                eprintln!("[vexec] mprotect RW failed for {:#x}: {}", page_start, *libc::__errno());
                continue;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, size);
            // Restore permissions — writable segments may overlap with RELRO pages
            // that were originally RWX after dlopen. The original permissions vary
            // per segment, but RW+EXEC is safe for .data/.bss (.got lives here too).
            if libc::mprotect(
                page_start as *mut c_void,
                page_size,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            ) != 0 {
                eprintln!("[vexec] mprotect RWX failed for {:#x}: {}", page_start, *libc::__errno());
            }
        }
    }
}

/// Patch GOT entries in a dlopen'd binary so PLT calls resolve to our interceptors.
///
/// After dlopen, the loaded binary's PLT stubs resolve through the GOT to libc's
/// real functions. We overwrite GOT entries for intercepted symbols (fork, execve,
/// pipe, etc.) so they point to our `#[no_mangle]` overrides instead.
fn patch_got_for_loaded_binary(base: usize, phdrs: &[elf::Phdr]) {
    let dyn_phdr = match phdrs.iter().find(|p| p.p_type == elf::PT_DYNAMIC) {
        Some(p) => p,
        None => return,
    };

    let dyn_addr = base + (dyn_phdr.p_vaddr as usize);
    let dyn_count = dyn_phdr.p_memsz as usize / std::mem::size_of::<elf::Dyn>();

    let mut jmprel: usize = 0;
    let mut pltrelsz: usize = 0;
    let mut symtab: usize = 0;
    let mut strtab: usize = 0;

    for i in 0..dyn_count {
        let dyn_ptr = (dyn_addr + i * std::mem::size_of::<elf::Dyn>()) as *const elf::Dyn;
        let tag: i64 = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*dyn_ptr).d_tag)) }.into();
        let val = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*dyn_ptr).d_val)) } as usize;
        match tag {
            elf::DT_JMPREL => jmprel = val,
            elf::DT_PLTRELSZ => pltrelsz = val,
            elf::DT_SYMTAB => symtab = val,
            elf::DT_STRTAB => strtab = val,
            _ => {}
        }
    }

    if jmprel == 0 || symtab == 0 || strtab == 0 || pltrelsz == 0 {
        return;
    }

    let jmprel = jmprel + base;
    let symtab = symtab + base;
    let strtab = strtab + base;

    let rela_count = pltrelsz / std::mem::size_of::<elf::Rela>();

    // Collect patches and unique pages for batch mprotect
    let mut patches: Vec<(*mut usize, usize)> = Vec::new();
    let mut pages = BTreeSet::new();

    for i in 0..rela_count {
        let rela_ptr = (jmprel + i * std::mem::size_of::<elf::Rela>()) as *const elf::Rela;
        let r_info = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*rela_ptr).r_info)) };
        let r_offset = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*rela_ptr).r_offset)) };

        if elf::rela_type(r_info) != elf::R_AARCH64_JUMP_SLOT {
            continue;
        }

        let sym_idx = elf::rela_sym(r_info) as usize;
        let sym_ptr = (symtab + sym_idx * std::mem::size_of::<elf::Sym>()) as *const elf::Sym;
        let st_name = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*sym_ptr).st_name)) } as usize;

        let name_cstr = (strtab + st_name) as *const std::os::raw::c_char;
        let name = match unsafe { std::ffi::CStr::from_ptr(name_cstr) }.to_str() {
            Ok(n) => n,
            Err(_) => continue,
        };

        let our_addr: usize = match name {
            "fork" | "vfork" => crate::preload::fork as *const c_void as usize,
            "execve" => crate::preload::execve as *const c_void as usize,
            "waitpid" => crate::preload::waitpid as *const c_void as usize,
            "wait4" => crate::preload::wait4 as *const c_void as usize,
            "pipe" => crate::preload::pipe as *const c_void as usize,
            "dup" => crate::preload::dup as *const c_void as usize,
            "dup2" => crate::preload::dup2 as *const c_void as usize,
            "close" => crate::preload::close as *const c_void as usize,
            "read" => crate::preload::read as *const c_void as usize,
            "write" => crate::preload::write as *const c_void as usize,
            "getpid" => crate::preload::getpid as *const c_void as usize,
            "getppid" => crate::preload::getppid as *const c_void as usize,
            "exit" => crate::preload::exit as *const c_void as usize,
            "_exit" => crate::preload::_exit as *const c_void as usize,
            "kill" => crate::preload::kill as *const c_void as usize,
            "getpgid" => crate::preload::getpgid as *const c_void as usize,
            "setpgid" => crate::preload::setpgid as *const c_void as usize,
            "raise" => crate::preload::raise as *const c_void as usize,
            _ => continue,
        };

        let got_entry = (base + r_offset as usize) as *mut usize;
        pages.insert((got_entry as usize) & !0xfff);
        patches.push((got_entry, our_addr));
    }

    if patches.is_empty() {
        return;
    }

    // Batch mprotect: make all unique pages writable
    for &page in &pages {
        unsafe {
            libc::mprotect(page as *mut c_void, 0x2000, libc::PROT_READ | libc::PROT_WRITE);
        }
    }

    // Apply all GOT patches
    for &(got_entry, our_addr) in &patches {
        unsafe { std::ptr::write_unaligned(got_entry, our_addr); }
    }

    // Restore GOT pages to read-only
    for &page in &pages {
        unsafe {
            libc::mprotect(page as *mut c_void, 0x2000, libc::PROT_READ);
        }
    }
}

/// Extract main() address from _start's instruction sequence.
///
/// On bionic aarch64, _start loads main's address into x2 via:
///   adrp x2, <page>
///   ... (other instructions setting x0, x1, x3)
///   ldr  x2, [x2, #<offset>]   // x2 = GOT entry for main
///   bl   __libc_init@plt
///
/// Returns the address of main(), or None if the pattern doesn't match.
unsafe fn extract_main_addr(entry_addr: usize) -> Option<usize> {
    let code = entry_addr as *const u32;
    // Scan first 48 instructions for adrp x2
    for i in 0..48 {
        let insn = std::ptr::read_unaligned(code.add(i));
        // ADRP x2: opcode 1xx1 0000, rd=2
        if (insn & 0x9F00001F) != 0x90000002 {
            continue;
        }
        let adrp_page = decode_adrp(entry_addr + i * 4, insn);
        // Scan forward for ldr x2, [x2, #imm]
        for j in (i + 1)..std::cmp::min(i + 8, 56) {
            let ldr_insn = std::ptr::read_unaligned(code.add(j));
            // LDR Xt, [Xn, #imm12]: 11 111 0 01 01 imm12(12) Rn(5) Rt(5)
            // With Rt=2 (Rn can be any register, typically x2 after adrp x2)
            if (ldr_insn & 0xFFC0001F) == 0xF9400002 {
                let imm12 = ((ldr_insn >> 10) & 0xFFF) as usize;
                let got_addr = adrp_page + imm12 * 8;
                let main_addr = std::ptr::read_unaligned(got_addr as *const usize);
                if main_addr != 0 && main_addr % 4 == 0 {
                    return Some(main_addr);
                }
            }
        }
    }
    None
}

/// Decode an ADRP instruction to get the target page address.
fn decode_adrp(pc: usize, insn: u32) -> usize {
    let immlo = (insn >> 29) & 0x3;
    let immhi = (insn >> 5) & 0x7FFFF;
    let imm = ((immhi << 2) | immlo) as i32;
    let imm = (imm << 12) >> 12; // sign extend 21-bit
    (pc & !0xFFF).wrapping_add((imm as usize) << 12)
}

/// Spawn a coroutine that directly calls main(argc, argv, envp).
/// Used when the binary has already been initialized via _start/__libc_init.
fn spawn_main_coroutine(
    main_addr: usize,
    argc: usize,
    argv: Vec<*const u8>,
    envp: Vec<*const u8>,
) -> VPid {
    crate::spawn(Box::new(move || {
        let main_fn: extern "C" fn(c_int, *const *const u8, *const *const u8) -> c_int =
            unsafe { std::mem::transmute(main_addr) };
        let result = main_fn(argc as c_int, argv.as_ptr(), envp.as_ptr());
        crate::executor::vproc_exit_with_code(result);
    }))
}

/// Write an inline-hook trampoline at `func_addr` that branches to `target`.
///
/// Overwrites the first 16 bytes with:
///   ldr x16, [pc, #8]   // load target address
///   br  x16             // branch to target
///   .quad target        // 64-bit target address
unsafe fn write_inline_hook(func_addr: usize, target: usize) -> bool {
    let page = func_addr & !0xfff;
    if libc::mprotect(
        page as *mut c_void,
        0x2000,
        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
    ) != 0 {
        return false;
    }

    let code = func_addr as *mut u32;
    std::ptr::write_unaligned(code, 0x58000050);       // ldr x16, [pc, #8]
    std::ptr::write_unaligned(code.add(1), 0xD61F0200); // br x16
    std::ptr::write_unaligned(code.add(2) as *mut usize, target);

    // Flush instruction cache for all 16 bytes (two 8-byte lines)
    for off in [0, 8] {
        std::arch::asm!(
            "dc cvau, {addr}",
            "dsb ish",
            "ic ivau, {addr}",
            "dsb ish",
            "isb",
            addr = in(reg) func_addr + off,
        );
    }
    true
}

/// Inline-hook libc's exit() by overwriting its first instructions
/// with a branch to our interceptor.
fn hook_libc_exit() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let rtld_next = -1isize as *mut c_void;
    unsafe {
        let original = libc::dlsym(rtld_next, b"exit\0".as_ptr() as *const std::os::raw::c_char);
        if original.is_null() { return; }
        write_inline_hook(original as usize, crate::preload::exit as *const c_void as usize);
    }
}

/// Inline-hook libc's execve() by overwriting its first instructions
/// with a branch to our interceptor.
fn hook_libc_execve() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let rtld_next = -1isize as *mut c_void;
    unsafe {
        let original = libc::dlsym(rtld_next, b"execve\0".as_ptr() as *const std::os::raw::c_char);
        if original.is_null() { return; }
        write_inline_hook(original as usize, crate::preload::execve as *const c_void as usize);
    }
}

/// Determine if a binary is static or dynamic and call the appropriate loader.
pub fn virtual_execve(
    path: &str,
    argv: Vec<String>,
    envp: Vec<String>,
) -> Result<VirtualExec, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read {}: {}", path, e))?;

    let hdr = elf::parse_header(&data)?;
    let phdrs = elf::program_headers(&data, &hdr)?;
    let has_interp = elf::interpreter_path(&data, &phdrs)?.is_some();

    if has_interp {
        // Dynamic binary: prefer entry point jump, fall back to dlsym("main")
        match virtual_execve_via_entry(path, argv.clone(), envp.clone()) {
            ok @ Ok(_) => ok,
            Err(_) => virtual_execve_dynamic(path, argv, envp),
        }
    } else {
        virtual_execve_static(path, argv, envp)
    }
}
