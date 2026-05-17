//! Virtual execve — load and execute ELF binaries inside coroutines.

use std::alloc::{alloc, Layout};
use std::ffi::c_int;

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
        virtual_execve_dynamic(path, argv, envp)
    } else {
        virtual_execve_static(path, argv, envp)
    }
}
