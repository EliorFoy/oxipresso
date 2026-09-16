use std::path::PathBuf;

use crate::PollingFileWatcher;

pub fn font_directories() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/System/Library/Fonts"),
        PathBuf::from("/Library/Fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    dirs
}

pub fn file_watcher(path: PathBuf) -> PollingFileWatcher {
    PollingFileWatcher::new(path)
}
