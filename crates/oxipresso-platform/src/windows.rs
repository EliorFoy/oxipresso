use std::{
    os::windows::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

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
/// Prefers the native `ReadDirectoryChangesW` watcher (event-driven, no
/// polling latency) and falls back to the portable [`PollingFileWatcher`]
/// when the directory cannot be opened for change notification (network
/// shares, exotic filesystems). The `Box<dyn FileWatcher>` interface keeps
/// the swap invisible to callers.
pub fn file_watcher(path: PathBuf) -> Box<dyn crate::FileWatcher> {
    match super::NativeReadDirectoryWatcher::new(&path) {
        Some(watcher) => Box::new(watcher),
        None => Box::new(PollingFileWatcher::new(path)),
    }
}

// ---------------------------------------------------------------------------
// native ReadDirectoryChangesW watcher (pull model)
//
// The `FileWatcher` trait is pull-based (`poll(&mut self)`), which dissolves
// all three gotchas recorded from the earlier background-thread attempt: the
// overlapped read completes on OUR event and we only `WaitForSingleObject`
// with zero timeout (ERROR_IO_PENDING is the normal queued state), there is
// no worker thread to join, and `Drop` is CancelIoEx + CloseHandle — it
// cannot hang during test panic-unwinds.

type Handle = *mut std::ffi::c_void;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x0000_0102;
const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x1 | 0x2 | 0x4;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x0000_0001;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x0000_0008;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x0000_0010;
const FILE_NOTIFY_CHANGE_CREATION: u32 = 0x0000_0040;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

unsafe extern "C" {
    fn CreateEventW(
        attributes: *const std::ffi::c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const std::ffi::c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn ReadDirectoryChangesW(
        directory: Handle,
        buffer: *mut u8,
        length: u32,
        watch_subtree: i32,
        notify_filter: u32,
        returned: *mut u32,
        overlapped: *mut Overlapped,
        completion_routine: Option<unsafe extern "C" fn()>,
    ) -> i32;
    fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    fn GetOverlappedResult(
        handle: Handle,
        overlapped: *mut Overlapped,
        returned: *mut u32,
        wait: i32,
    ) -> i32;
    fn CancelIoEx(handle: Handle, overlapped: *mut Overlapped) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
}

/// Watches the directory containing `path` and reports `Changed` when the
/// watched file's name appears in a change record. Pull-based: nothing runs
/// on other threads; `poll` never blocks.
pub struct NativeReadDirectoryWatcher {
    path: PathBuf,
    file_name: String,
    dir: Handle,
    event: Handle,
    overlapped: Overlapped,
    buffer: Box<[u8; 16 * 1024]>,
    // Whether an overlapped read is outstanding.
    pending: bool,
}

impl NativeReadDirectoryWatcher {
    /// Returns `None` when the change-notification watch cannot be
    /// established (the caller falls back to the polling watcher).
    pub fn new(path: &Path) -> Option<Self> {
        let parent = path.parent()?.to_path_buf();
        let file_name = path.file_name()?.to_string_lossy().to_lowercase();
        unsafe {
            let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            if event.is_null() || event == INVALID_HANDLE_VALUE {
                return None;
            }
            let parent_wide: Vec<u16> = parent
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let dir = CreateFileW(
                parent_wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ_WRITE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            );
            if dir == INVALID_HANDLE_VALUE || dir.is_null() {
                CloseHandle(event);
                return None;
            }
            let mut watcher = Self {
                path: path.to_path_buf(),
                file_name,
                dir,
                event,
                overlapped: Overlapped {
                    internal: 0,
                    internal_high: 0,
                    offset: 0,
                    offset_high: 0,
                    event,
                },
                buffer: Box::new([0u8; 16 * 1024]),
                pending: false,
            };
            if watcher.issue_read().is_none() {
                watcher.close_handles();
                return None;
            }
            Some(watcher)
        }
    }

    /// Issue one overlapped directory read. `None` on immediate failure.
    fn issue_read(&mut self) -> Option<()> {
        unsafe {
            let ok = ReadDirectoryChangesW(
                self.dir,
                self.buffer.as_mut_ptr(),
                self.buffer.len() as u32,
                0, // watch_subtree: the artifact lives directly in this dir
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_CREATION,
                std::ptr::null_mut(),
                &mut self.overlapped,
                None,
            );
            if ok == 0 && GetLastError() != 997 {
                // 997 = ERROR_IO_PENDING, the normal queued-completion state
                // for an overlapped call - the read is now outstanding.
                return None;
            }
            self.pending = true;
            Some(())
        }
    }

    fn close_handles(&mut self) {
        unsafe {
            if self.pending {
                CancelIoEx(self.dir, &mut self.overlapped);
            }
            if !self.dir.is_null() {
                CloseHandle(self.dir);
            }
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
        }
        self.pending = false;
    }

    fn drain_records(&mut self, bytes_returned: u32) -> bool {
        // Walk FILE_NOTIFY_INFORMATION records; `true` when any record names
        // the watched file (any action counts: modified/removed/added all
        // invalidate the artifact).
        let data = &self.buffer[..(bytes_returned as usize).min(self.buffer.len())];
        let mut offset = 0usize;
        let mut matched = false;
        loop {
            if offset + 12 > data.len() {
                break;
            }
            let next = u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            let name_len =
                u32::from_ne_bytes(data[offset + 8..offset + 12].try_into().unwrap()) as usize;
            if name_len >= 2 && offset + 12 + name_len <= data.len() {
                let name_bytes = &data[offset + 12..offset + 12 + name_len];
                let name: String = name_bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_ne_bytes(pair.try_into().unwrap()))
                    .take_while(|&c| c != 0)
                    .filter_map(|c| char::from_u32(u32::from(c)))
                    .collect();
                if name.to_lowercase() == self.file_name {
                    matched = true;
                }
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
        matched
    }
}

impl crate::FileWatcher for NativeReadDirectoryWatcher {
    fn path(&self) -> &Path {
        &self.path
    }

    fn poll(&mut self) -> crate::FileWatchEvent {
        // The file vanishing dominates every other signal.
        if crate::current_modified(&self.path).is_none() {
            return crate::FileWatchEvent::Missing {
                path: self.path.clone(),
            };
        }
        unsafe {
            let state = WaitForSingleObject(self.event, 0);
            if state == WAIT_OBJECT_0 {
                let mut returned: u32 = 0;
                let ok = GetOverlappedResult(self.dir, &mut self.overlapped, &mut returned, 0);
                self.pending = false;
                let changed = ok != 0 && self.drain_records(returned);
                // Re-arm immediately so later changes are still observed.
                if self.issue_read().is_none() {
                    return crate::FileWatchEvent::Missing {
                        path: self.path.clone(),
                    };
                }
                if changed {
                    return crate::FileWatchEvent::Changed {
                        path: self.path.clone(),
                        modified: crate::current_modified(&self.path),
                    };
                }
                return crate::FileWatchEvent::Unchanged;
            }
            if state == WAIT_TIMEOUT {
                return crate::FileWatchEvent::Unchanged;
            }
            // Unexpected wait failure: report the file state via metadata.
            match crate::current_modified(&self.path) {
                Some(modified) => crate::FileWatchEvent::Changed {
                    path: self.path.clone(),
                    modified: Some(modified),
                },
                None => crate::FileWatchEvent::Missing {
                    path: self.path.clone(),
                },
            }
        }
    }

    fn mark_clean(&mut self, _modified: Option<std::time::SystemTime>) {
        // Event-driven: the baseline is the directory change stream itself;
        // nothing to store (kept for the trait contract).
    }
}

impl Drop for NativeReadDirectoryWatcher {
    fn drop(&mut self) {
        self.close_handles();
    }
}
