use std::path::PathBuf;

use crate::PollingFileWatcher;

pub fn font_directories() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/fonts"));
    }
    dirs
}

pub fn file_watcher(path: PathBuf) -> PollingFileWatcher {
    PollingFileWatcher::new(path)
}
