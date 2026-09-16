use std::path::PathBuf;

use crate::PollingFileWatcher;

pub fn font_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    dirs
}

pub fn file_watcher(path: PathBuf) -> PollingFileWatcher {
    PollingFileWatcher::new(path)
}
