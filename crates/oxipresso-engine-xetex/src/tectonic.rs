//! Tectonic package provider (P3, local-bundle subset).
//!
//! Tectonic ships TeX content from "bundles". The full original supports
//! network bundles (indexed zip archives fetched over HTTP); this module
//! implements the LOCAL subset — a directory laid out as a texmf tree, the
//! same shape tectonic's own `DirBundle` uses for offline setups. Resolution
//! is a plain path join: `<bundle>/<relative path>`, with the `FileKind`
//! used only to guess a missing extension (TFM lookups arrive extensionless
//! in some code paths).
//!
//! Network bundles are deliberately out of scope here (silent-divergence
//! risk without a bundle index; see AGENTS.md). `-tectonic` selects this
//! provider and requires `OXIPRESSO_TECTONIC_BUNDLE` to name the directory.

use oxipresso_engine_api::{FileKind, FileResolver};
use std::path::PathBuf;

pub const BUNDLE_ENV: &str = "OXIPRESSO_TECTONIC_BUNDLE";

/// A resolver over a local directory bundle.
pub struct DirBundleResolver {
    root: PathBuf,
}

impl DirBundleResolver {
    /// The bundle directory named by [`BUNDLE_ENV`], or `None` when unset.
    pub fn from_env() -> Option<Self> {
        let root = std::env::var_os(BUNDLE_ENV)?;
        Some(Self {
            root: PathBuf::from(root),
        })
    }

    /// Whether the provider is usable (the env names an existing directory).
    pub fn check_env() -> Result<(), String> {
        match std::env::var_os(BUNDLE_ENV) {
            Some(root) => {
                let root = PathBuf::from(root);
                if root.is_dir() {
                    Ok(())
                } else {
                    Err(format!("{BUNDLE_ENV} ({root:?}) is not a directory"))
                }
            }
            None => Err(format!(
                "the Tectonic package provider needs {BUNDLE_ENV} pointing at a \
                 local bundle directory (texmf layout); network bundles are not \
                 supported"
            )),
        }
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FileResolver for DirBundleResolver {
    fn resolve(&mut self, path: &str, kind: FileKind) -> Option<Vec<u8>> {
        // Bundle lookups are relative texmf paths; absolute paths or parent
        // traversals escape the bundle and must never resolve.
        let rel = path.replace('\\', "/");
        if rel.starts_with('/') || rel.split('/').any(|part| part == "..") {
            return None;
        }
        let mut candidates = vec![self.root.join(&rel)];
        // TFM lookups can arrive without an extension (kind-specific guessing
        // mirrors tectonic's own extension probing).
        if kind == FileKind::Tfm && !rel.contains('.') {
            candidates.push(self.root.join(format!("{rel}.tfm")));
        }
        candidates
            .into_iter()
            .find_map(|candidate| std::fs::read(&candidate).ok())
    }
}
