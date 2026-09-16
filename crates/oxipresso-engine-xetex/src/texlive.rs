//! TeX Live provider: resolves TeX distribution files through `kpsewhich`,
//! mirroring the original TeXpresso `-texlive` provider. Editor buffers and
//! VFS disk roots always take precedence; this resolver only sees paths the
//! VFS could not satisfy otherwise.

use std::{collections::HashSet, path::PathBuf, process::Command};

use oxipresso_engine_api::{FileKind, FileResolver};

/// Resolves TeX distribution files by shelling out to `kpsewhich`
/// (TeX Live / TinyTeX). Resolved files are read from disk once and cached by
/// the VFS; failed lookups are remembered negatively to avoid respawning the
/// helper for files the distribution does not have.
pub struct KpsewhichResolver {
    program: PathBuf,
    program_version: String,
    failed_lookups: HashSet<String>,
}

impl KpsewhichResolver {
    pub fn from_program(program: impl Into<PathBuf>) -> Option<Self> {
        let program = program.into();
        let output = Command::new(&program).arg("--version").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let program_version = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        Some(Self {
            program,
            program_version,
            failed_lookups: HashSet::new(),
        })
    }

    /// Detects `kpsewhich` on `PATH`. Disabled when `OXIPRESSO_TEXLIVE=0`.
    pub fn auto() -> Option<Self> {
        if std::env::var("OXIPRESSO_TEXLIVE").ok().as_deref() == Some("0") {
            return None;
        }
        Self::from_program("kpsewhich")
    }

    pub fn program_version(&self) -> &str {
        &self.program_version
    }

    /// File kinds that must never be resolved from the TeX distribution:
    /// engine-owned outputs (formats) and the editor-owned primary document.
    fn resolvable_kind(kind: FileKind) -> bool {
        !matches!(
            kind,
            FileKind::Format | FileKind::Primary | FileKind::ProgramData
        )
    }
}

impl FileResolver for KpsewhichResolver {
    fn resolve(&mut self, path: &str, kind: FileKind) -> Option<Vec<u8>> {
        if !Self::resolvable_kind(kind) {
            return None;
        }
        if self.failed_lookups.contains(path) {
            return None;
        }
        let output = Command::new(&self.program).arg(path).output().ok()?;
        if !output.status.success() {
            self.failed_lookups.insert(path.to_string());
            return None;
        }
        let resolved = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?
            .to_string();
        match std::fs::read(&resolved) {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                self.failed_lookups.insert(path.to_string());
                None
            }
        }
    }
}
