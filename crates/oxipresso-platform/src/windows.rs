use std::path::PathBuf;

use crate::PollingFileWatcher;

/// Mirrors an environment variable into the MSVC CRT's cached environment
/// block so native C `getenv` callers observe it (see the crate-level
/// `set_process_env`). `std::env::set_var` alone is invisible to the CRT.
pub fn set_crt_env(name: &str, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "C" {
        fn _wputenv_s(name: *const u16, value: *const u16) -> std::os::raw::c_int;
    }
    let mut name_wide: Vec<u16> = name.encode_utf16().collect();
    name_wide.push(0);
    let mut value_wide: Vec<u16> = value.encode_wide().collect();
    value_wide.push(0);
    unsafe {
        _wputenv_s(name_wide.as_ptr(), value_wide.as_ptr());
    }
}

pub fn font_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    dirs
}

/// Platform watcher factory (Windows).
///
/// Currently returns the portable [`PollingFileWatcher`]. A native
/// `ReadDirectoryChangesW` watcher was attempted and removed because raw
/// overlapped-Win32 cancellation was not yet reliable here (see AGENTS.md
/// notes); the `Box<dyn FileWatcher>` interface lets it be swapped in later
/// without touching any caller.
pub fn file_watcher(path: PathBuf) -> Box<dyn crate::FileWatcher> {
    Box::new(PollingFileWatcher::new(path))
}
