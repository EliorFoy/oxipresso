use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::NativeReadDirectoryWatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
    Windows,
    Linux,
    Macos,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformInfo {
    pub kind: PlatformKind,
    pub path_separator: char,
}

pub fn current_platform() -> PlatformInfo {
    PlatformInfo {
        kind: platform_kind(),
        path_separator: std::path::MAIN_SEPARATOR,
    }
}

pub fn canonicalize_for_display(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy();
    // On Windows, std::fs::canonicalize returns an extended-length path
    // (`\\?\C:\...`, or `\\?\UNC\server\share` for network shares). That prefix
    // is meaningless to display and would survive the separator swap as `//?/`.
    // Strip it (harmless on Unix, where no path begins with `\\?\`).
    let stripped = match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!("\\\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    };
    stripped.replace('\\', "/")
}

pub fn font_directories() -> Vec<PathBuf> {
    platform_font_directories()
}

/// Sets a process environment variable so that both Rust and native C code
/// (calling `getenv`) observe it.
///
/// On Windows, the MSVC CRT snapshots the environment block at startup, so
/// `std::env::set_var` (which calls `SetEnvironmentVariableW`) is invisible to
/// C `getenv` callers; this function additionally mirrors the value into the
/// CRT block via `_wputenv_s`. This is the single place that OS quirk lives so
/// engine adapters stay platform-neutral.
///
/// # Safety
/// Mutates the process-global environment. Concurrent readers (including C
/// code calling `getenv`) may observe a torn state; call during startup before
/// native code begins reading the environment.
pub unsafe fn set_process_env(name: &str, value: &std::ffi::OsStr) {
    unsafe {
        std::env::set_var(name, value);
    }
    platform_set_crt_env(name, value);
}

#[cfg(windows)]
fn platform_set_crt_env(name: &str, value: &std::ffi::OsStr) {
    windows::set_crt_env(name, value);
}
#[cfg(not(windows))]
fn platform_set_crt_env(_name: &str, _value: &std::ffi::OsStr) {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileWatchEvent {
    Unchanged,
    Missing {
        path: PathBuf,
    },
    Changed {
        path: PathBuf,
        modified: Option<SystemTime>,
    },
}

pub trait FileWatcher {
    fn path(&self) -> &Path;
    fn poll(&mut self) -> FileWatchEvent;
    fn mark_clean(&mut self, modified: Option<SystemTime>);
}

#[derive(Debug, Clone)]
pub struct PollingFileWatcher {
    path: PathBuf,
    observed_modified: Option<SystemTime>,
}

impl PollingFileWatcher {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let observed_modified = current_modified(&path);
        Self {
            path,
            observed_modified,
        }
    }

    pub fn observed_modified(&self) -> Option<SystemTime> {
        self.observed_modified
    }
}

impl FileWatcher for PollingFileWatcher {
    fn path(&self) -> &Path {
        &self.path
    }

    fn poll(&mut self) -> FileWatchEvent {
        let Some(modified) = current_modified(&self.path) else {
            return FileWatchEvent::Missing {
                path: self.path.clone(),
            };
        };
        if Some(modified) == self.observed_modified {
            return FileWatchEvent::Unchanged;
        }
        FileWatchEvent::Changed {
            path: self.path.clone(),
            modified: Some(modified),
        }
    }

    fn mark_clean(&mut self, modified: Option<SystemTime>) {
        self.observed_modified = modified;
    }
}

pub fn file_watcher(path: impl Into<PathBuf>) -> Box<dyn FileWatcher> {
    platform_file_watcher(path.into())
}

fn current_modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

#[cfg(windows)]
fn platform_kind() -> PlatformKind {
    PlatformKind::Windows
}
#[cfg(target_os = "linux")]
fn platform_kind() -> PlatformKind {
    PlatformKind::Linux
}
#[cfg(target_os = "macos")]
fn platform_kind() -> PlatformKind {
    PlatformKind::Macos
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_kind() -> PlatformKind {
    PlatformKind::Other
}

#[cfg(windows)]
fn platform_font_directories() -> Vec<PathBuf> {
    windows::font_directories()
}
#[cfg(target_os = "linux")]
fn platform_font_directories() -> Vec<PathBuf> {
    linux::font_directories()
}
#[cfg(target_os = "macos")]
fn platform_font_directories() -> Vec<PathBuf> {
    macos::font_directories()
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_font_directories() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(windows)]
fn platform_file_watcher(path: PathBuf) -> Box<dyn FileWatcher> {
    windows::file_watcher(path)
}
#[cfg(target_os = "linux")]
fn platform_file_watcher(path: PathBuf) -> Box<dyn FileWatcher> {
    linux::file_watcher(path)
}
#[cfg(target_os = "macos")]
fn platform_file_watcher(path: PathBuf) -> Box<dyn FileWatcher> {
    macos::file_watcher(path)
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_file_watcher(path: PathBuf) -> Box<dyn FileWatcher> {
    Box::new(PollingFileWatcher::new(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// The native ReadDirectoryChangesW watcher (Windows): a write to the
    /// watched file surfaces as Changed on the next poll, an unrelated file
    /// in the same directory stays Unchanged, and deleting the file reports
    /// Missing. Skipped when the native watch cannot be established (e.g.
    /// filesystems without change notification) so the polling fallback is
    /// exercised instead on those systems.
    #[cfg(windows)]
    #[test]
    fn native_watcher_reports_changes_of_the_watched_file_only() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.xdv");
        std::fs::write(&path, b"first").unwrap();
        let Some(mut watcher) = NativeReadDirectoryWatcher::new(&path) else {
            eprintln!("native watcher unavailable on this filesystem; skipping");
            return;
        };
        // Baseline: nothing has changed since the watch was established.
        assert!(matches!(watcher.poll(), FileWatchEvent::Unchanged));

        // Unrelated sibling change: must NOT invalidate the watched file.
        std::fs::write(dir.join("other.tmp"), b"noise").unwrap();
        assert!(matches!(watcher.poll(), FileWatchEvent::Unchanged));

        // Watched-file write: Changed on the next poll (coalesced if the
        // write happened before the previous poll drained it).
        std::fs::write(&path, b"second version").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_changed = false;
        while std::time::Instant::now() < deadline {
            match watcher.poll() {
                FileWatchEvent::Changed { path: changed, .. } => {
                    assert_eq!(changed, path);
                    saw_changed = true;
                    break;
                }
                FileWatchEvent::Unchanged => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(saw_changed, "watched-file write never surfaced as Changed");

        // Deletion: Missing (the dominant signal, even mid-event-stream).
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(watcher.poll(), FileWatchEvent::Missing { .. }));
    }

    #[test]
    fn polling_watcher_reports_created_file_without_marking_clean() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.pdf");
        let mut watcher = PollingFileWatcher::new(&path);
        assert!(matches!(watcher.poll(), FileWatchEvent::Missing { .. }));

        std::fs::write(&path, b"%PDF-1.7").unwrap();
        let first = watcher.poll();
        let second = watcher.poll();

        assert!(matches!(first, FileWatchEvent::Changed { .. }));
        assert!(matches!(second, FileWatchEvent::Changed { .. }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn polling_watcher_mark_clean_suppresses_same_timestamp() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.pdf");
        std::fs::write(&path, b"%PDF-1.7").unwrap();
        let mut watcher = PollingFileWatcher::new(&path);

        let modified = std::fs::metadata(&path).unwrap().modified().ok();
        watcher.mark_clean(modified);

        assert_eq!(watcher.poll(), FileWatchEvent::Unchanged);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn canonicalize_for_display_strips_extended_length_prefix() {
        // Windows std::fs::canonicalize prepends `\\?\`; the display helper must
        // not surface it as a `//?/` (or any leading `//`) path. On Unix the
        // temp dir has no such prefix, so the assertions hold everywhere.
        let display = canonicalize_for_display(std::env::temp_dir());
        assert!(
            !display.starts_with("//?/"),
            "extended-length prefix leaked into display path: {display}"
        );
        assert!(
            !display.starts_with("//"),
            "expected a normalized display path, got: {display}"
        );
        assert!(
            display.contains('/'),
            "expected forward-slash display path: {display}"
        );
    }

    fn unique_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "oxipresso-platform-watch-test-{}-{nonce}",
            std::process::id()
        ))
    }
}
