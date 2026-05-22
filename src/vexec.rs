//! Virtual execve — load and execute ELF binaries inside coroutines.

use std::collections::{BTreeSet, HashMap};
use std::ffi::c_int;
use std::sync::Arc;
use std::os::raw::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::coroutine::VPid;
use crate::elf;
use crate::loader;

const ELF_STACK_SIZE: usize = 8 * 1024 * 1024; // 8 MiB for loaded binaries

/// Check if MTE (Memory Tagging Extension) is available — cached after first call.
fn mte_available() -> bool {
    use std::sync::atomic::{AtomicI8, Ordering as Ord2};
    static CACHED: AtomicI8 = AtomicI8::new(-1);
    let v = CACHED.load(Ord2::Relaxed);
    if v >= 0 { return v != 0; }
    let hwcap = unsafe { libc::getauxval(libc::AT_HWCAP) };
    let has = (hwcap & (1 << 18)) != 0; // HWCAP_MTE
    CACHED.store(has as i8, Ord2::Relaxed);
    has
}

/// Allocate an ELF execution stack with guard page.
/// PROT_MTE disabled — can cause MTE async SIGKILL on Android 16 untrusted_app.
fn alloc_elf_stack(size: usize) -> *mut u8 {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    let guard_size = page_size;
    let total = guard_size + size;
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            total,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1, 0,
        )
    };
    if base == libc::MAP_FAILED { return std::ptr::null_mut(); }
    let stack_start = unsafe { (base as *mut u8).add(guard_size) };
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    // PROT_MTE disabled — causes MTE async SIGKILL on Android 16 untrusted_app
    // since ELF stack data is not properly tagged by the virtualized environment.
    let r = unsafe {
        libc::mmap(
            stack_start as *mut c_void,
            size,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1, 0,
        )
    };
    if r == libc::MAP_FAILED {
        unsafe { libc::munmap(base, total); }
        return std::ptr::null_mut();
    }
    stack_start
}

/// Free an ELF stack allocated by alloc_elf_stack.
pub fn free_elf_stack(ptr: *mut u8, size: usize) {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    unsafe { libc::munmap(ptr.sub(page_size) as *mut c_void, page_size + size); }
}

/// mprotect wrapper — currently passes through without PROT_MTE.
/// MTE tagging disabled to avoid async SIGKILL on Android 16 untrusted_app.
fn mprotect_mte_aware(addr: usize, size: usize, base_prot: c_int) -> c_int {
    unsafe { libc::mprotect(addr as *mut c_void, size, base_prot) }
}

struct DlHandle(*mut c_void);
unsafe impl Send for DlHandle {}
unsafe impl Sync for DlHandle {}

/// RAII guard that dlclose's a dlopen handle on drop.
/// Used to prevent handle leaks on early error returns.
struct DlGuard(*mut c_void);
impl Drop for DlGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { libc::dlclose(self.0); }
        }
    }
}

struct BinaryCacheEntry {
    main_addr: usize,
    handle: DlHandle,
    /// Saved writable segment contents and their original mprotect permissions.
    /// Wrapped in Arc so cache lookups don't clone the entire snapshot.
    saved_writable: Arc<Vec<WritableSegment>>,
    /// Number of coroutines currently executing code from this binary.
    /// When this drops to zero, the entry is eligible for dlclose.
    /// Relaxed is sufficient: BINARY_CACHE Mutex provides happens-before
    /// for all fetch_add / fetch_sub operations on this field.
    active_users: AtomicU32,
}

struct WritableSegment {
    addr: usize,
    data: Vec<u8>,
    /// Original permissions from PT_LOAD flags (PF_R|PF_W|PF_X).
    orig_flags: u32,
}

/// Cache of loaded binary metadata per canonicalized path.
/// After the first dlopen + _start run, we save main()'s address and a snapshot
/// of all writable segments. Subsequent calls restore writable state so the
/// binary's globals are fresh, then call main() directly — skipping _start
/// entirely.
static BINARY_CACHE: std::sync::LazyLock<Mutex<HashMap<String, BinaryCacheEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Remove a binary from the cache and dlclose its handle.
///
/// # Safety
/// Must only be called when no coroutine is executing code from this binary.
/// Calling while a coroutine is inside the binary's code is undefined behavior.
pub fn unload_binary(path: &str) -> bool {
    let real_path = match std::fs::canonicalize(path) {
        Ok(p) => p.to_str().map(|s| s.to_string()),
        Err(_) => Some(path.to_string()),
    };
    let real_path = match real_path {
        Some(p) => p,
        None => return false,
    };
    let mut cache = BINARY_CACHE.lock().unwrap();
    match cache.remove(&real_path) {
        Some(entry) => {
            if !entry.handle.0.is_null() {
                unsafe { libc::dlclose(entry.handle.0); }
            }
            true
        }
        None => false,
    }
}

/// Remove all binaries from the cache and dlclose their handles.
///
/// # Safety
/// Must only be called when no coroutine is executing code from any cached binary.
pub fn unload_all_binaries() {
    let mut cache = BINARY_CACHE.lock().unwrap();
    for (_, entry) in cache.drain() {
        if !entry.handle.0.is_null() {
            unsafe { libc::dlclose(entry.handle.0); }
        }
    }
}

/// Return the number of cached binaries.
pub fn cached_binary_count() -> usize {
    BINARY_CACHE.lock().unwrap().len()
}

/// Release references to binaries used by finished coroutines.
/// Decrements active_users for each path. Entries are kept alive for reuse
/// across sessions — only dlclose when explicitly requested via unload_binary.
/// NOTE: do NOT dlclose here — it triggers __libc_init which resets minicoro's
/// _mco_main_ctx, breaking subsequent coroutine context switching.
pub fn release_binaries(paths: &[String]) {
    let cache = BINARY_CACHE.lock().unwrap();
    for path in paths {
        if let Some(entry) = cache.get(path) {
            entry.active_users.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Associate a spawned coroutine with its cached binary and increment the refcount.
fn track_binary_user(vpid: VPid, path: &str) {
    let ex = unsafe { &mut *crate::executor::get_current_executor() };
    if let Some(co) = ex.vprocs.get_mut(&vpid) {
        co.binary_path = Some(path.to_string());
    }
    let mut cache = BINARY_CACHE.lock().unwrap();
    if let Some(entry) = cache.get_mut(path) {
        entry.active_users.fetch_add(1, Ordering::Relaxed);
    }
}

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

    // Build C strings (cleaned up when coroutine is dropped)
    let (argv_c, mut argv_raw) = build_c_strings(&argv);
    let (envp_c, envp_raw) = build_c_strings(&envp);
    argv_raw.extend(envp_raw);

    // Build auxiliary vector
    let auxv = loader::build_auxv(&image, 0);

    // Allocate stack (mmap + PROT_MTE + guard page)
    let stack_base = alloc_elf_stack(ELF_STACK_SIZE);
    if stack_base.is_null() {
        return Err("stack allocation failed".into());
    }

    // Spawn ELF coroutine
    let vpid = unsafe {
        (*crate::executor::get_current_executor()).spawn_elf(
            image.entry,
            stack_base,
            ELF_STACK_SIZE,
            argv_c.len(),
            argv_c,
            envp_c,
            auxv,
        )
    };
    register_elf_c_strings(vpid, argv_raw);
    unsafe {
        (*crate::executor::get_current_executor())
            .register_mapped_region(vpid, image.base, image.total_size);
    }

    Ok(VirtualExec { vpid })
}
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

    // Build C strings (cleaned up when coroutine is dropped)
    let (argv_c, mut c_strings) = build_c_strings(&argv);
    let (_envp_c, envp_raw) = build_c_strings(&envp);
    c_strings.extend(envp_raw);

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
    unsafe {
        (*crate::executor::get_current_executor()).register_c_strings(vpid, c_strings);
    }

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

    // Build C strings (cleaned up when coroutine is dropped)
    let (argv_c, mut c_strings) = build_c_strings(&argv);
    let (envp_c, envp_raw) = build_c_strings(&envp);
    c_strings.extend(envp_raw);
    let argc = argv_c.len();

    // Check if we already have main() cached for this binary
    let current_pid = unsafe { (*crate::executor::get_current_executor()).current };
    let cached = {
        let lock = BINARY_CACHE.lock().unwrap();
        lock.get(&real_path_str).map(|e| (e.main_addr, Arc::clone(&e.saved_writable), e.handle.0))
    };

    if let Some((main_addr, saved_writable, _handle)) = cached {
        if main_addr == 0 {
            // main() address couldn't be extracted on first load — this binary
            // is not compatible. Remove the cache entry and dlclose the handle
            // since no coroutine will ever reference it (active_users == 0).
            let mut cache = BINARY_CACHE.lock().unwrap();
            if let Some(entry) = cache.remove(&real_path_str) {
                if !entry.handle.0.is_null() {
                    unsafe { libc::dlclose(entry.handle.0); }
                }
            }
            return Err(format!("cannot extract main() from {}", path));
        }
        // Binary already initialized — restore writable segments and re-patch GOT,
        // then call main() directly, skipping _start/__libc_init.
        restore_writable_segments(&saved_writable);

        // Re-read ELF to get phdrs for GOT re-patching after restore.
        // The snapshot was taken before _start ran, so __libc_init may have
        // modified GOT entries (e.g. resolved lazy bindings). Restore brings
        // back the pre-_start GOT, so we must re-apply our hooks.
        let re_data = std::fs::read(&real_path)
            .map_err(|e| format!("cannot read {}: {}", path, e))?;
        let re_hdr = elf::parse_header(&re_data)?;
        let re_phdrs = elf::program_headers(&re_data, &re_hdr)?;
        let re_base = find_loaded_base(path).ok_or("cannot find re-loaded base")?;
        patch_got_for_loaded_binary(re_base, &re_phdrs);

        let vpid = spawn_main_coroutine(main_addr, argc, argv_c, envp_c, c_strings);
        track_binary_user(vpid, &real_path_str);
        // Inherit fd table from current coroutine (Linux execve preserves fds)
        if let Some(pid) = current_pid {
            crate::vfd::fork_fd_table(pid, vpid);
        }
        // Close fds marked close-on-exec in the new process
        if let Some(child_table) = crate::vfd::get_table(vpid) {
            let real_fds = child_table.close_cloexec();
            for rfd in real_fds {
                unsafe { crate::preload::real_close(rfd); }
            }
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
    // Guard dlclose's on early return; forgotten when handle is cached.
    let guard = DlGuard(handle);

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
    // Always cache the entry so the handle is tracked for auto-dlclose.
    // main_addr=0 means we couldn't extract main() — cache-hit path will
    // skip the optimized main() call and fall through to _start instead.
    let main_addr = extracted.unwrap_or(0);
    let saved = save_writable_segments(base, &phdrs);
    BINARY_CACHE.lock().unwrap().insert(real_path_str.clone(), BinaryCacheEntry {
        main_addr,
        handle: DlHandle(handle),
        saved_writable: Arc::new(saved),
        active_users: AtomicU32::new(0),
    });
    // Handle is now owned by the cache — prevent guard from dlclose'ing it.
    std::mem::forget(guard);

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

    let stack_base = alloc_elf_stack(ELF_STACK_SIZE);
    if stack_base.is_null() {
        return Err("stack allocation failed".into());
    }

    let vpid = unsafe {
        let ex = &mut *crate::executor::get_current_executor();
        // Pin executor to global pointer so it survives TLS reinitialization
        // when __libc_init runs inside the dlopen'd binary's _start.
        crate::executor::set_current_executor(ex as *mut _);
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
    register_elf_c_strings(vpid, c_strings);
    track_binary_user(vpid, &real_path_str);

    // Inherit fd table from current coroutine (Linux execve preserves fds)
    if let Some(pid) = current_pid {
        crate::vfd::fork_fd_table(pid, vpid);
    }
    // Close fds marked close-on-exec in the new process
    if let Some(child_table) = crate::vfd::get_table(vpid) {
        let real_fds = child_table.close_cloexec();
        for rfd in real_fds {
            unsafe { crate::preload::real_close(rfd); }
        }
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
/// Records original PT_LOAD flags so restore can set correct permissions.
fn save_writable_segments(base: usize, phdrs: &[elf::Phdr]) -> Vec<WritableSegment> {
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
        segments.push(WritableSegment {
            addr,
            data: slice.to_vec(),
            orig_flags: phdr.p_flags,
        });
    }
    segments
}

/// Restore writable segment contents from a saved snapshot.
/// Re-mprotects pages writable, writes data, then restores original permissions
/// derived from PT_LOAD flags.
fn restore_writable_segments(segments: &[WritableSegment]) {
    for seg in segments {
        let size = seg.data.len();
        if size == 0 {
            continue;
        }
        let page_start = seg.addr & !0xfff;
        let page_end = (seg.addr + size + 0xfff) & !0xfff;
        let page_size = page_end - page_start;
        unsafe {
            // Make writable for restore
            if mprotect_mte_aware(
                page_start,
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
            ) != 0 {
                eprintln!("[vexec] mprotect RW failed for {:#x}: {}", page_start, *libc::__errno());
                continue;
            }
            std::ptr::copy_nonoverlapping(seg.data.as_ptr(), seg.addr as *mut u8, size);

            // Restore to original permissions from PT_LOAD flags
            let mut prot = 0u32;
            if seg.orig_flags & elf::PF_R != 0 { prot |= libc::PROT_READ as u32; }
            if seg.orig_flags & elf::PF_W != 0 { prot |= libc::PROT_WRITE as u32; }
            if seg.orig_flags & elf::PF_X != 0 { prot |= libc::PROT_EXEC as u32; }
            // Writable segments containing GOT need at least RW for future
            // lazy binding resolution; ensure W is preserved.
            if prot & (libc::PROT_WRITE as u32) == 0 {
                prot |= libc::PROT_WRITE as u32;
            }
            if mprotect_mte_aware(
                page_start,
                page_size,
                prot as c_int,
            ) != 0 {
                eprintln!("[vexec] mprotect restore failed for {:#x}: {}", page_start, *libc::__errno());
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
            "open" => crate::preload::open as *const c_void as usize,
            "openat" => crate::preload::openat as *const c_void as usize,
            "creat" => crate::preload::creat as *const c_void as usize,
            "fstat" => crate::preload::fstat as *const c_void as usize,
            "lseek" => crate::preload::lseek as *const c_void as usize,
            "chdir" => crate::preload::chdir as *const c_void as usize,
            "getcwd" => crate::preload::getcwd as *const c_void as usize,
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
        mprotect_mte_aware(page, 0x2000, libc::PROT_READ | libc::PROT_WRITE);
    }

    // Apply all GOT patches
    for &(got_entry, our_addr) in &patches {
        unsafe { std::ptr::write_unaligned(got_entry, our_addr); }
    }

    // Restore GOT pages to read-only
    for &page in &pages {
        mprotect_mte_aware(page, 0x2000, libc::PROT_READ);
    }
}

/// Extract main() address from _start's instruction sequence.
///
/// On bionic aarch64, _start loads main's address into x2 via:
///   adrp xN, <page>
///   add  xN, xN, #<addend>     (optional, for large offsets)
///   ...
///   ldr  x2, [xN, #<offset>]   // x2 = GOT entry for main
///   bl   __libc_init
///
/// We scan for `ldr x2, [xN, #imm]` (Rt=2, any Rn), then search backward
/// for the matching `adrp xN` (and optional `add xN, xN, #imm`) to compute
/// the correct GOT address.
unsafe fn extract_main_addr(entry_addr: usize) -> Option<usize> {
    let code = entry_addr as *const u32;
    // Scan first 48 instructions for ldr x2, [xN, #imm12]
    for i in 0..48 {
        let insn = std::ptr::read_unaligned(code.add(i));
        // LDR Xt, [Xn, #imm12]: 11 111 0 01 01 imm12(12) Rn(5) Rt(5)
        // Rt must be 2 (x2), Rn can be any register
        if (insn & 0xFFC0001F) != 0xF9400002 {
            continue;
        }
        let rn = ((insn >> 5) & 0x1F) as u32;
        let ldr_imm12 = ((insn >> 10) & 0xFFF) as usize;

        // Search backward for adrp xN
        for j in (0..i).rev() {
            let adrp_insn = std::ptr::read_unaligned(code.add(j));
            // ADRP xN: opcode 1xx1 0000, rd=N
            if (adrp_insn & 0x9F000000) != 0x90000000 {
                continue;
            }
            if (adrp_insn & 0x1F) != rn {
                continue;
            }
            let adrp_page = decode_adrp(entry_addr + j * 4, adrp_insn);

            // Check for add xN, xN, #imm12 between adrp and ldr (adrp+add pair)
            let mut addend: usize = 0;
            for k in (j + 1)..i {
                let mid = std::ptr::read_unaligned(code.add(k));
                // ADD Xd, Xn, #imm12: 1 00 100010 0 imm12(12) Rn(5) Rd(5)
                if (mid & 0xFFC00000) == 0x91000000
                    && (mid & 0x1F) == rn
                    && ((mid >> 5) & 0x1F) == rn
                {
                    addend = ((mid >> 10) & 0xFFF) as usize;
                }
            }

            let got_addr = adrp_page + addend + ldr_imm12 * 8;
            let main_addr = std::ptr::read_unaligned(got_addr as *const usize);
            if main_addr != 0 && main_addr % 4 == 0 {
                return Some(main_addr);
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
    c_strings: Vec<*mut u8>,
) -> VPid {
    let vpid = crate::spawn(Box::new(move || {
        let main_fn: extern "C" fn(c_int, *const *const u8, *const *const u8) -> c_int =
            unsafe { std::mem::transmute(main_addr) };
        // Leak Vec buffers — bionic stores argv/environ pointers internally
        // and they must remain valid for the process lifetime. The C string
        // data is tracked via c_strings and cleaned up on coroutine drop.
        let argv_ptr = argv.as_ptr();
        let envp_ptr = envp.as_ptr();
        std::mem::forget(argv);
        std::mem::forget(envp);
        let result = main_fn(argc as c_int, argv_ptr, envp_ptr);
        crate::executor::vproc_exit_with_code(result);
    }));
    unsafe {
        (*crate::executor::get_current_executor()).register_c_strings(vpid, c_strings);
    }
    vpid
}

/// Convert Rust strings to C strings, returning both the pointer vec for use
/// and the raw pointers for later cleanup.
fn build_c_strings(strings: &[String]) -> (Vec<*const u8>, Vec<*mut u8>) {
    let mut ptrs = Vec::with_capacity(strings.len());
    let mut raw = Vec::with_capacity(strings.len());
    for s in strings {
        let cs = std::ffi::CString::new(s.as_str()).unwrap();
        let r = cs.into_raw();
        ptrs.push(r as *const u8);
        raw.push(r);
    }
    (ptrs, raw)
}

/// Register C strings for cleanup when a coroutine spawned via spawn_elf is dropped.
fn register_elf_c_strings(vpid: VPid, c_strings: Vec<*mut u8>) {
    unsafe {
        (*crate::executor::get_current_executor()).register_c_strings(vpid, c_strings);
    }
}

/// Write an inline-hook trampoline at `func_addr` that branches to `target`.
///
/// Overwrites the first 16 bytes with:
///   ldr x16, [pc, #8]   // load target address
///   br  x16             // branch to target
///   .quad target        // 64-bit target address
unsafe fn write_inline_hook(func_addr: usize, target: usize) -> bool {
    let page = func_addr & !0xfff;
    if mprotect_mte_aware(
        page,
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
