use std::path::PathBuf;

use crate::PollingFileWatcher;

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
