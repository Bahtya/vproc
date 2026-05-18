//! Virtual execve — load and execute ELF binaries inside coroutines.

use std::alloc::{alloc, Layout};
use std::ffi::c_int;
use std::os::raw::c_void;

use crate::coroutine::VPid;
use crate::elf;
use crate::loader;

const ELF_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MiB for loaded binaries

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
    // Parse ELF header to get e_entry
    let data = std::fs::read(path)
        .map_err(|e| format!("cannot read {}: {}", path, e))?;
    let hdr = elf::parse_header(&data)?;
    let e_entry = hdr.e_entry as usize;
    let phdrs = elf::program_headers(&data, &hdr)?;

    // dlopen — dynamic linker loads all dependencies and resolves relocations
    let c_path = std::ffi::CString::new(path).map_err(|e| e.to_string())?;
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        let err = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) }
            .to_string_lossy()
            .into_owned();
        return Err(format!("dlopen({}): {}", path, err));
    }

    // Find loaded base address via dl_iterate_phdr
    let base = find_loaded_base(path).ok_or_else(|| format!(
        "dlopen({}) succeeded but dl_iterate_phdr cannot find it", path
    ))?;

    let entry_addr = base + e_entry;

    // Clear DT_INIT_ARRAY / DT_INIT to prevent double constructor execution
    // (dlopen already called them, _start would call them again)
    clear_init_arrays(base, &phdrs);

    // Inline-hook libc's exit() and _exit() so that when __libc_init calls
    // exit(result), it goes to our interceptor instead of killing the process.
    // GOT patching the binary alone isn't enough because __libc_init is in
    // libc.so and calls exit() directly (not through PLT).
    hook_libc_exit();

    // Build C strings (leaked — must survive coroutine lifetime)
    let argv_c: Vec<*const u8> = argv
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect();
    let envp_c: Vec<*const u8> = envp
        .iter()
        .map(|s| std::ffi::CString::new(s.as_str()).unwrap().into_raw() as *const u8)
        .collect();

    // Build auxv for the loaded binary
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

    // Allocate stack
    let stack_layout = Layout::from_size_align(ELF_STACK_SIZE, 16)
        .map_err(|e| e.to_string())?;
    let stack_base = unsafe { alloc(stack_layout) };
    if stack_base.is_null() {
        return Err("stack allocation failed".into());
    }

    // Spawn ELF coroutine that jumps to the entry point
    let vpid = crate::executor::EXECUTOR.with(|e| unsafe {
        let ex = &mut *e.get();
        // Set global executor pointer so it survives TLS reinitialization
        // by __libc_init inside the loaded binary
        crate::executor::set_global_executor(ex as *mut _);
        ex.spawn_elf(
            entry_addr,
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

/// Inline-hook libc's exit() and _exit() by overwriting their first instructions
/// with a branch to our interceptors.
///
/// When a loaded binary's _start → __libc_init calls exit(), it goes through
/// libc.so's internal call (not PLT). GOT patching only works for the loaded
/// binary's PLT calls. We need to intercept at the libc level by patching
/// the function code itself.
///
/// On aarch64, we write a trampoline at the function entry:
///   ldr x16, [pc, #8]    // load target address from literal pool
///   br  x16              // branch to target
///   .quad target_addr    // 8-byte literal
fn hook_libc_exit() {
    let rtld_next = -1isize as *mut c_void;
    unsafe {
        let targets: &[(&[u8], *const c_void)] = &[
            (b"exit\0", crate::preload::exit as *const c_void),
        ];

        for &(name, target) in targets {
            let name_c = std::ffi::CStr::from_ptr(name.as_ptr() as *const std::os::raw::c_char);
            let original = libc::dlsym(rtld_next, name_c.as_ptr());
            if original.is_null() { continue; }

            let func_addr = original as usize;
            let page = func_addr & !0xfff;

            let ret = libc::mprotect(
                page as *mut c_void,
                0x2000,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            );
            if ret != 0 { continue; }

            // Write trampoline:
            //   ldr x16, [pc, #8]   → 0x58000050
            //   br  x16             → 0xD61F0200
            //   .quad target_addr
            let code = func_addr as *mut u32;
            std::ptr::write_unaligned(code, 0x58000050);
            std::ptr::write_unaligned(code.add(1), 0xD61F0200);
            let target_ptr = code.add(2) as *mut usize;
            std::ptr::write_unaligned(target_ptr, target as usize);

            std::arch::asm!(
                "dc cvau, {addr}",
                "dsb ish",
                "ic ivau, {addr}",
                "dsb ish",
                "isb",
                addr = in(reg) func_addr,
            );
        }
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
