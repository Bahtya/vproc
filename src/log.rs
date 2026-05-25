//! Lightweight logcat logging via runtime-resolved `__android_log_print`.
//!
//! Zero new crate dependencies — uses `dlopen`/`dlsym` on `liblog.so`.

use std::sync::OnceLock;

type LogFn = unsafe extern "C" fn(c_int, *const u8, *const u8, *const u8) -> c_int;
use std::os::raw::c_int;

const LOG_DEBUG: c_int = 3;
const LOG_ERROR: c_int = 6;

static LOGGER: OnceLock<Option<LogFn>> = OnceLock::new();

fn get_log_fn() -> Option<LogFn> {
    *LOGGER.get_or_init(|| unsafe {
        let handle = libc::dlopen(c"liblog.so".as_ptr() as *const _, libc::RTLD_NOW);
        if handle.is_null() {
            return None;
        }
        let sym = libc::dlsym(handle, c"__android_log_print".as_ptr() as *const _);
        if sym.is_null() {
            return None;
        }
        Some(std::mem::transmute::<*mut libc::c_void, LogFn>(sym))
    })
}

/// Log a debug-level message to Android logcat (tag "vproc").
/// Silently does nothing if liblog.so is unavailable.
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        $crate::log::emit($crate::log::Level::Debug, &msg);
    }};
}

/// Log an error-level message to Android logcat (tag "vproc").
#[macro_export]
macro_rules! vlog_error {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        $crate::log::emit($crate::log::Level::Error, &msg);
    }};
}

#[derive(Clone, Copy)]
pub enum Level {
    Debug,
    Error,
}

/// Write `msg` to Android logcat.  Falls back to `libc::write(2, ...)` when
/// `liblog.so` is unavailable (e.g. running tests on a Linux host).
pub fn emit(level: Level, msg: &str) {
    let prio = match level {
        Level::Debug => LOG_DEBUG,
        Level::Error => LOG_ERROR,
    };
    if let Some(log_fn) = get_log_fn() {
        // CString::new fails on embedded NUL — use lossy conversion as fallback
        let c_msg = std::ffi::CString::new(msg)
            .unwrap_or_else(|_| std::ffi::CString::new(msg.replace('\0', "?")).unwrap_or_default());
        let tag = b"vproc\0";
        let fmt = b"%s\0";
        unsafe { log_fn(prio, tag.as_ptr(), fmt.as_ptr(), c_msg.as_ptr()); }
    }
}
