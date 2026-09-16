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
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn font_directories() -> Vec<PathBuf> {
    platform_font_directories()
}

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

pub fn file_watcher(path: impl Into<PathBuf>) -> PollingFileWatcher {
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
fn platform_file_watcher(path: PathBuf) -> PollingFileWatcher {
    windows::file_watcher(path)
}
#[cfg(target_os = "linux")]
fn platform_file_watcher(path: PathBuf) -> PollingFileWatcher {
    linux::file_watcher(path)
}
#[cfg(target_os = "macos")]
fn platform_file_watcher(path: PathBuf) -> PollingFileWatcher {
    macos::file_watcher(path)
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn platform_file_watcher(path: PathBuf) -> PollingFileWatcher {
    PollingFileWatcher::new(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn unique_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oxipresso-platform-watch-test-{nonce}"))
    }
}
