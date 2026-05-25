//! Lightweight logcat logging via runtime-resolved `__android_log_print`.
//!
//! Zero new crate dependencies — uses `dlopen`/`dlsym` on `liblog.so`.

use std::sync::OnceLock;

type LogFn = unsafe extern "C" fn(c_int, *const u8, *const u8, ...) -> c_int;
use std::os::raw::c_int;

const ANDROID_LOG_DEBUG: c_int = 3;
const ANDROID_LOG_ERROR: c_int = 6;

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
        $crate::log::_emit($crate::log::_Level::Debug, &msg);
    }};
}

/// Log an error-level message to Android logcat (tag "vproc").
#[macro_export]
macro_rules! vlog_error {
    ($($arg:tt)*) => {{
        let msg = format!($($arg)*);
        $crate::log::_emit($crate::log::_Level::Error, &msg);
    }};
}

#[derive(Clone, Copy)]
pub enum _Level {
    Debug,
    Error,
}

pub fn _emit(level: _Level, msg: &str) {
    let prio = match level {
        _Level::Debug => 3, // ANDROID_LOG_DEBUG
        _Level::Error => 6, // ANDROID_LOG_ERROR
    };
    if let Some(log_fn) = get_log_fn() {
        let tag = b"vproc\0";
        let c_msg = std::ffi::CString::new(msg).unwrap_or_default();
        unsafe { log_fn(prio, tag.as_ptr(), c_msg.as_ptr()); }
    }
}
