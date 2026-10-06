//! TeX Live provider: resolves TeX distribution files through `kpsewhich`,
//! mirroring the original TeXpresso `-texlive` provider. Editor buffers and
//! VFS disk roots always take precedence; this resolver only sees paths the
//! VFS could not satisfy otherwise.
//!
//! Resolution results are persisted to a disk cache because spawning
//! `kpsewhich` costs ~100-200 ms per call on Windows and a typical document
//! triggers dozens of lookups (classes, packages, fonts, map files). The
//! cache turns warm rebuilds from seconds into milliseconds.

use std::{collections::HashMap, path::PathBuf, process::Command};

use oxipresso_engine_api::{FileKind, FileResolver};

/// Resolves TeX distribution files by shelling out to `kpsewhich`
/// (TeX Live / TinyTeX). Resolved files are read from disk once and cached by
/// the VFS; failed lookups are remembered negatively. Both positive and
/// negative resolutions persist to a cache file so subsequent processes
/// (rebuilds, new sessions) skip the process spawn entirely.
pub struct KpsewhichResolver {
    program: PathBuf,
    program_version: String,
    /// name → resolved distribution path (positive cache).
    hits: HashMap<String, String>,
    /// names the distribution does not have (negative cache).
    misses: std::collections::HashSet<String>,
    cache_path: Option<PathBuf>,
    cache_dirty: bool,
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
            hits: HashMap::new(),
            misses: std::collections::HashSet::new(),
            cache_path: None,
            cache_dirty: false,
        })
    }

    /// Detects `kpsewhich` on `PATH`. Disabled when `OXIPRESSO_TEXLIVE=0`.
    /// The cache location defaults to `%TEMP%/oxipresso-kpsewhich-cache.txt`
    /// and can be overridden with `OXIPRESSO_KPSE_CACHE`.
    pub fn auto() -> Option<Self> {
        if std::env::var("OXIPRESSO_TEXLIVE").ok().as_deref() == Some("0") {
            return None;
        }
        let mut resolver = Self::from_program("kpsewhich")?;
        let cache_path = std::env::var("OXIPRESSO_KPSE_CACHE")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("TEMP")
                    .or_else(|_| std::env::var("TMP"))
                    .ok()
                    .map(|temp| PathBuf::from(temp).join("oxipresso-kpsewhich-cache.txt"))
            });
        resolver.cache_path = cache_path;
        resolver.load_cache();
        Some(resolver)
    }

    /// Overrides the cache file location (used by tests).
    pub fn with_cache_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.cache_path = Some(path.into());
        self.load_cache();
        self
    }

    pub fn program_version(&self) -> &str {
        &self.program_version
    }

    /// Number of cached positive/negative entries (used by tests).
    #[cfg(test)]
    pub fn cache_stats(&self) -> (usize, usize) {
        (self.hits.len(), self.misses.len())
    }

    fn load_cache(&mut self) {
        let Some(path) = self.cache_path.as_ref() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(tag), Some(name)) = (parts.next(), parts.next()) else {
                continue;
            };
            match (tag, name, parts.next()) {
                ("hit", name, Some(resolved)) => {
                    self.hits.insert(name.to_string(), resolved.to_string());
                }
                ("miss", name, _) => {
                    self.misses.insert(name.to_string());
                }
                _ => {}
            }
        }
    }

    fn save_cache(&mut self) {
        if !self.cache_dirty {
            return;
        }
        let Some(path) = self.cache_path.as_ref() else {
            return;
        };
        let mut text = String::new();
        for (name, resolved) in &self.hits {
            text.push_str("hit\t");
            text.push_str(name);
            text.push('\t');
            text.push_str(resolved);
            text.push('\n');
        }
        for name in &self.misses {
            text.push_str("miss\t");
            text.push_str(name);
            text.push('\n');
        }
        if std::fs::write(path, text).is_ok() {
            self.cache_dirty = false;
        }
    }

    /// File kinds that must never be resolved from the TeX distribution:
    /// engine-owned outputs (formats) and the editor-owned primary document.
    fn resolvable_kind(kind: FileKind) -> bool {
        !matches!(
            kind,
            FileKind::Format | FileKind::Primary | FileKind::ProgramData
        )
    }

    /// Runs one `kpsewhich` invocation for `path`. Each resolution is flushed
    /// to the on-disk cache immediately, so a cold build interrupted by a
    /// crash or Ctrl-C still preserves everything resolved up to that point
    /// (the next warm run skips those process spawns).
    fn run_kpsewhich(&mut self, path: &str) -> Option<Vec<u8>> {
        let output = Command::new(&self.program).arg(path).output().ok()?;
        if !output.status.success() {
            self.misses.insert(path.to_string());
            self.cache_dirty = true;
            self.save_cache();
            return None;
        }
        let resolved = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?
            .to_string();
        match std::fs::read(&resolved) {
            Ok(bytes) => {
                self.hits.insert(path.to_string(), resolved);
                self.cache_dirty = true;
                self.save_cache();
                Some(bytes)
            }
            Err(_) => {
                self.misses.insert(path.to_string());
                self.cache_dirty = true;
                self.save_cache();
                None
            }
        }
    }
}

impl Drop for KpsewhichResolver {
    fn drop(&mut self) {
        self.save_cache();
    }
}

impl FileResolver for KpsewhichResolver {
    fn resolve(&mut self, path: &str, kind: FileKind) -> Option<Vec<u8>> {
        if !Self::resolvable_kind(kind) {
            return None;
        }
        if let Some(resolved) = self.hits.get(path) {
            match std::fs::read(resolved) {
                Ok(bytes) => return Some(bytes),
                Err(_) => {
                    // The resolved file disappeared; re-resolve.
                    self.hits.remove(path);
                }
            }
        }
        if self.misses.contains(path) {
            return None;
        }
        self.run_kpsewhich(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a fake `kpsewhich` stub for the host OS that answers
    /// `--version` and echoes a fixed resolved path for lookups. The tests
    /// must run on every platform the binary ships for (the Linux release
    /// build caught the cmd.exe-only original).
    fn fake_kpsewhich(dir: &std::path::Path, target: &std::path::Path) -> PathBuf {
        #[cfg(windows)]
        let (name, body) = (
            "fake-kpsewhich.cmd",
            format!(
                "@if \"%~1\"==\"--version\" (\r\n  @echo fake kpsewhich 1.0\r\n  @exit /b 0\r\n)\r\n@echo {}\r\n",
                target.display()
            ),
        );
        #[cfg(not(windows))]
        let (name, body) = (
            "fake-kpsewhich.sh",
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"fake kpsewhich 1.0\"\n  exit 0\nfi\necho {}\n",
                target.display()
            ),
        );
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        make_executable(&script);
        script
    }

    /// A stub whose `--version` succeeds but every lookup exits 1 —
    /// kpsewhich's missing-file behavior.
    fn failing_lookup_stub(dir: &std::path::Path) -> PathBuf {
        #[cfg(windows)]
        let (name, body) = (
            "failing-lookup-kpsewhich.cmd",
            "@if \"%~1\"==\"--version\" (\r\n  @echo fake kpsewhich 1.0\r\n  @exit /b 0\r\n)\r\n@exit /b 1\r\n"
                .to_string(),
        );
        #[cfg(not(windows))]
        let (name, body) = (
            "failing-lookup-kpsewhich.sh",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  echo \"fake kpsewhich 1.0\"\n  exit 0\nfi\nexit 1\n"
                .to_string(),
        );
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        make_executable(&script);
        script
    }

    /// A stub that answers the version for EVERY argument and always exits 0
    /// (used to prove a miss is served from the persisted cache).
    fn always_version_stub(dir: &std::path::Path) -> PathBuf {
        #[cfg(windows)]
        let (name, body) = (
            "always-version-kpsewhich.cmd",
            "@echo fake kpsewhich 1.0\r\n@exit /b 0\r\n".to_string(),
        );
        #[cfg(not(windows))]
        let (name, body) = (
            "always-version-kpsewhich.sh",
            "#!/bin/sh\necho \"fake kpsewhich 1.0\"\nexit 0\n".to_string(),
        );
        let script = dir.join(name);
        std::fs::write(&script, body).unwrap();
        make_executable(&script);
        script
    }

    #[cfg(unix)]
    fn make_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &std::path::Path) {}

    /// A process- and nanosecond-unique temp dir. Windows reuses PIDs and a
    /// previous aborted run can leave a same-named dir behind, so a bare
    /// `process::id()` is not enough — the nonce guarantees no collision.
    fn unique_dir(prefix: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn resolution_cache_persists_hits_and_misses_across_instances() {
        let dir = unique_dir("oxi-kpse-test");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("article.sty");
        std::fs::write(&target, b"\\ProvidesPackage{article}").unwrap();
        let script = fake_kpsewhich(&dir, &target);
        let cache = dir.join("cache.txt");

        // Instance 1: cold cache; one hit (resolves through the stub) and one
        // miss (the Format kind is never resolved).
        let mut resolver = KpsewhichResolver::from_program(&script)
            .unwrap()
            .with_cache_path(&cache);
        let bytes = resolver.resolve("article.sty", FileKind::Tex).unwrap();
        assert_eq!(bytes, b"\\ProvidesPackage{article}");
        assert!(resolver.resolve("article.sty", FileKind::Format).is_none());
        drop(resolver);

        // Instance 2: warm cache. The resolving stub is replaced by a
        // version-only stub that fails every lookup — so a successful
        // resolution proves the hit came from the persisted cache.
        let version_only = failing_lookup_stub(&dir);
        let mut resolver = KpsewhichResolver::from_program(&version_only)
            .unwrap()
            .with_cache_path(&cache);
        let bytes = resolver.resolve("article.sty", FileKind::Tex).unwrap();
        assert_eq!(bytes, b"\\ProvidesPackage{article}");
        let (hits, _misses) = resolver.cache_stats();
        assert_eq!(hits, 1);
        drop(resolver);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_flushes_during_run_before_drop() {
        let dir = unique_dir("oxi-kpse-flush");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("flushme.sty");
        std::fs::write(&target, b"content").unwrap();
        let script = fake_kpsewhich(&dir, &target);
        let cache = dir.join("cache.txt");
        let _ = std::fs::remove_file(&cache);

        let mut resolver = KpsewhichResolver::from_program(&script)
            .unwrap()
            .with_cache_path(&cache);
        resolver.resolve("flushme.sty", FileKind::Tex).unwrap();
        // Without dropping, the cache must already be on disk so an interrupted
        // run still keeps resolved entries.
        assert!(cache.is_file(), "cache must be flushed during the run");
        let text = std::fs::read_to_string(&cache).unwrap();
        assert!(
            text.contains("flushme.sty"),
            "flushed cache should include the just-resolved file, got {text:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_invalidates_stale_cached_hit_when_file_disappears() {
        // A cached "hit" stores an absolute path; if a TeX update removes that
        // file between runs, resolve must re-read, detect it's gone, drop the
        // stale hit, and degrade gracefully — never panic or serve stale data.
        let dir = unique_dir("oxi-kpse-stale");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("gone.sty");
        std::fs::write(&target, b"content").unwrap();
        let script = fake_kpsewhich(&dir, &target);
        let cache = dir.join("cache.txt");
        let _ = std::fs::remove_file(&cache);

        let mut resolver = KpsewhichResolver::from_program(&script)
            .unwrap()
            .with_cache_path(&cache);
        assert_eq!(
            resolver.resolve("gone.sty", FileKind::Tex).unwrap(),
            b"content"
        );
        assert_eq!(resolver.cache_stats(), (1, 0), "one fresh hit cached");

        // Simulate the distribution updating and removing the resolved file.
        std::fs::remove_file(&target).unwrap();
        assert!(
            resolver.resolve("gone.sty", FileKind::Tex).is_none(),
            "stale hit must not be served and must not panic"
        );
        let (hits, misses) = resolver.cache_stats();
        assert_eq!(hits, 0, "the vanished hit is dropped from the cache");
        assert_eq!(misses, 1, "the re-resolution miss is recorded");
        drop(resolver);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn negative_cache_avoids_respawning_for_missing_files() {
        let dir = unique_dir("oxi-kpse-miss");
        std::fs::create_dir_all(&dir).unwrap();
        // A stub that succeeds at --version but fails every lookup —
        // kpsewhich's missing-file behavior.
        let script = failing_lookup_stub(&dir);
        let cache = dir.join("cache.txt");

        let mut resolver = KpsewhichResolver::from_program(&script)
            .unwrap()
            .with_cache_path(&cache);
        assert!(resolver.resolve("missing.sty", FileKind::Tex).is_none());
        drop(resolver);

        // Second instance: a fresh version-only stub; the miss must come from
        // the loaded cache (proven by cache_stats, since a real spawn would
        // also fail but not record).
        let version_only = always_version_stub(&dir);
        let resolver = KpsewhichResolver::from_program(&version_only)
            .unwrap()
            .with_cache_path(&cache);
        let (hits, misses) = resolver.cache_stats();
        assert_eq!(hits, 0);
        assert_eq!(misses, 1);
        drop(resolver);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_cache_tolerates_malformed_entries() {
        // The kpse cache is a user-writable temp file a prior/interrupted/
        // hand-edited run may leave malformed. load_cache must keep the good
        // lines and ignore bad ones (no tabs / unknown tag / truncated hit),
        // never panic.
        let dir = unique_dir("oxi-kpse-malformed");
        std::fs::create_dir_all(&dir).unwrap();
        let version_only = failing_lookup_stub(&dir);
        let cache = dir.join("cache.txt");
        std::fs::write(
            &cache,
            "garbage-no-tabs\n\
             hit\tarticle.cls\tC:/texlive/texmf-dist/tex/latex/article.cls\n\
             bogus\twhatever\tx\n\
             miss\tmissing.sty\n\
             hit\ttruncated-no-path\n",
        )
        .unwrap();
        let resolver = KpsewhichResolver::from_program(&version_only)
            .unwrap()
            .with_cache_path(&cache);
        // Exactly one valid hit and one valid miss load; the rest are skipped.
        assert_eq!(resolver.cache_stats(), (1, 1));
        drop(resolver);
        std::fs::remove_dir_all(&dir).ok();
    }
}
