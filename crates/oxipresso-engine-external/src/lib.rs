use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use oxipresso_engine_api::{
    ArtifactKind, Diagnostic, DiagnosticSeverity, DocumentArtifact, EngineError, EngineEvent,
    EngineInit, EngineIo, FileKind, OpenResult, PathId, RestartPolicy, Result, RootDocument,
    SyncTexArtifact, TypesettingEngine,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEngineConfig {
    pub command: String,
}

impl Default for ExternalEngineConfig {
    fn default() -> Self {
        Self {
            command: "xelatex".to_string(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ExternalEngine {
    config: ExternalEngineConfig,
    diagnostics: Vec<Diagnostic>,
    output: Option<DocumentArtifact>,
    synctex: Option<SyncTexArtifact>,
    last_root: Option<RootDocument>,
}

impl ExternalEngine {
    pub fn new(config: ExternalEngineConfig) -> Self {
        Self {
            config,
            diagnostics: Vec::new(),
            output: None,
            synctex: None,
            last_root: None,
        }
    }

    pub fn xelatex() -> Self {
        Self::new(ExternalEngineConfig::default())
    }

    pub fn is_available(command: &str) -> bool {
        Command::new(command)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn compile(&mut self, root: &RootDocument, io: &mut dyn EngineIo) -> Result<()> {
        self.diagnostics.clear();
        self.output = None;
        self.synctex = None;

        let build_dir = unique_build_dir()?;
        fs::create_dir_all(&build_dir)?;
        materialize_snapshot_inputs(io, &build_dir)?;
        let root_bytes = read_root_bytes(root, io)?;
        // root_name can carry a subdirectory (e.g. "src/main.tex"), so create the
        // parent first - matching materialize_snapshot_inputs; a bare fs::write
        // into a missing subdir fails and aborts the whole external compile.
        let temp_root = write_into_build_dir(&build_dir, &root.root_name, &root_bytes)?;

        let texinputs = texinputs_for(root, &build_dir);
        let output = Command::new(&self.config.command)
            .arg("-interaction=nonstopmode")
            .arg("-halt-on-error")
            .arg("-file-line-error")
            .arg("-synctex=1")
            .arg("-output-directory")
            .arg(&build_dir)
            .arg(&temp_root)
            .current_dir(&root.root_dir)
            .env("TEXINPUTS", texinputs)
            .output()
            .map_err(|e| {
                EngineError::new(format!("failed to launch {}: {e}", self.config.command))
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.is_empty() {
            self.diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Info,
                message: stdout.to_string(),
                path: None,
                line: None,
            });
        }
        if !stderr.is_empty() {
            self.diagnostics.push(Diagnostic {
                severity: DiagnosticSeverity::Warning,
                message: stderr.to_string(),
                path: None,
                line: None,
            });
        }

        let stem = Path::new(&root.root_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| EngineError::new("root file has no valid stem"))?;
        let pdf_path = build_dir.join(format!("{stem}.pdf"));
        if output.status.success() && pdf_path.exists() {
            self.output = Some(DocumentArtifact {
                kind: ArtifactKind::Pdf,
                bytes: fs::read(&pdf_path)?,
                source_name: Some(pdf_path.to_string_lossy().to_string()),
            });
            self.synctex = read_synctex_artifact(&build_dir, stem)?;
            let _ = fs::remove_dir_all(&build_dir);
            return Ok(());
        }

        let log_path = build_dir.join(format!("{stem}.log"));
        if log_path.exists() {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            if !log.is_empty() {
                self.diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Error,
                    message: log,
                    path: None,
                    line: None,
                });
            }
        }
        let _ = fs::remove_dir_all(&build_dir);
        Err(EngineError::new(format!(
            "{} failed with status {}",
            self.config.command, output.status
        )))
    }
}

impl TypesettingEngine for ExternalEngine {
    fn initialize(&mut self, root: &RootDocument, io: &mut dyn EngineIo) -> Result<EngineInit> {
        self.last_root = Some(root.clone());
        self.compile(root, io)?;
        Ok(EngineInit {
            engine_name: format!("external-{}", self.config.command),
        })
    }

    fn step(&mut self, _io: &mut dyn EngineIo) -> Result<EngineEvent> {
        Ok(match self.output.clone() {
            Some(artifact) => EngineEvent::DocumentUpdated(artifact),
            None => EngineEvent::Terminated,
        })
    }

    fn apply_change_hint(
        &mut self,
        _changed_file: &PathId,
        _byte_offset: usize,
    ) -> Result<RestartPolicy> {
        Ok(RestartPolicy::FullRestartRequired)
    }

    fn restart(&mut self, io: &mut dyn EngineIo) -> Result<()> {
        let root = self
            .last_root
            .clone()
            .ok_or_else(|| EngineError::new("external engine has not been initialized"))?;
        self.compile(&root, io)
    }

    fn output_document(&self) -> Option<DocumentArtifact> {
        self.output.clone()
    }

    fn output_synctex(&self) -> Option<SyncTexArtifact> {
        self.synctex.clone()
    }

    fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

fn read_root_bytes(root: &RootDocument, io: &mut dyn EngineIo) -> Result<Vec<u8>> {
    match io.open_read(&root.root_name, FileKind::Tex)? {
        OpenResult::Opened { handle, .. } => {
            let mut offset = 0;
            let mut bytes = Vec::new();
            loop {
                let chunk = io.read(handle, offset, 64 * 1024)?;
                if chunk.is_empty() {
                    break;
                }
                offset += chunk.len();
                bytes.extend_from_slice(&chunk);
            }
            io.seen(handle, offset, 0);
            io.close(handle)?;
            Ok(bytes)
        }
        OpenResult::Missing | OpenResult::Promised => {
            let path = root.root_dir.join(&root.root_name);
            Ok(fs::read(path)?)
        }
    }
}

fn unique_build_dir() -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| EngineError::new(e.to_string()))?
        .as_nanos();
    Ok(std::env::temp_dir().join(format!("oxipresso-{}-{now}", std::process::id())))
}

fn write_into_build_dir(build_dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let target = build_dir.join(name);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, bytes)?;
    Ok(target)
}
fn read_synctex_artifact(build_dir: &Path, stem: &str) -> Result<Option<SyncTexArtifact>> {
    let gz_path = build_dir.join(format!("{stem}.synctex.gz"));
    if gz_path.exists() {
        return Ok(Some(SyncTexArtifact {
            bytes: fs::read(&gz_path)?,
            compressed: true,
            source_name: Some(gz_path.to_string_lossy().to_string()),
        }));
    }

    let path = build_dir.join(format!("{stem}.synctex"));
    if path.exists() {
        return Ok(Some(SyncTexArtifact {
            bytes: fs::read(&path)?,
            compressed: false,
            source_name: Some(path.to_string_lossy().to_string()),
        }));
    }

    Ok(None)
}

fn materialize_snapshot_inputs(io: &mut dyn EngineIo, build_dir: &Path) -> Result<()> {
    for (path, bytes) in io.snapshot_inputs()? {
        let Some(relative_path) = safe_snapshot_relative_path(&path) else {
            continue;
        };
        let target = build_dir.join(relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, bytes)?;
    }
    Ok(())
}

fn safe_snapshot_relative_path(path: &str) -> Option<PathBuf> {
    let relative = PathBuf::from(path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if relative.is_absolute() {
        return None;
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(relative)
}

fn texinputs_for(root: &RootDocument, build_dir: &Path) -> String {
    let separator = if cfg!(windows) { ';' } else { ':' };
    let mut paths = Vec::new();
    paths.push(root.root_dir.clone());
    paths.push(build_dir.to_path_buf());
    paths.extend(root.include_paths.iter().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            root.root_dir.join(path)
        }
    }));

    let mut value = paths
        .into_iter()
        .map(|path| {
            let mut text = path.to_string_lossy().replace('\\', "/");
            if !text.ends_with('/') {
                text.push('/');
            }
            text
        })
        .collect::<Vec<_>>()
        .join(&separator.to_string());
    value.push(separator);
    if let Some(existing) = env::var_os("TEXINPUTS")
        && !existing.is_empty()
    {
        value.push_str(&existing.to_string_lossy());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxipresso_engine_api::EngineIo;
    use std::{env, fs};

    #[test]
    fn write_into_build_dir_creates_missing_parent_dirs() {
        // A root whose name carries a subdirectory ("sub/main.tex") used to fail
        // the bare fs::write and abort the external compile; the helper must
        // create the parent. build_dir itself is not pre-created here.
        let dir = unique_build_dir().unwrap();
        let written = write_into_build_dir(&dir, "sub/main.tex", b"hello").unwrap();
        assert!(written.exists(), "subdir root file must be created");
        assert_eq!(fs::read_to_string(&written).unwrap(), "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    struct MemoryIo {
        bytes: Vec<u8>,
        snapshots: Vec<(String, Vec<u8>)>,
    }

    impl EngineIo for MemoryIo {
        fn open_read(&mut self, _path: &str, _kind: FileKind) -> Result<OpenResult> {
            Ok(OpenResult::Opened {
                handle: oxipresso_engine_api::FileHandle(1),
                canonical_path: "main.tex".to_string(),
            })
        }
        fn open_write(
            &mut self,
            _path: &str,
            _kind: FileKind,
        ) -> Result<oxipresso_engine_api::FileHandle> {
            unreachable!()
        }
        fn read(
            &mut self,
            _handle: oxipresso_engine_api::FileHandle,
            offset: usize,
            len: usize,
        ) -> Result<Vec<u8>> {
            Ok(self.bytes[offset..usize::min(offset + len, self.bytes.len())].to_vec())
        }
        fn size(&mut self, _handle: oxipresso_engine_api::FileHandle) -> Result<usize> {
            Ok(self.bytes.len())
        }
        fn append(
            &mut self,
            _handle: oxipresso_engine_api::FileHandle,
            _bytes: &[u8],
        ) -> Result<()> {
            unreachable!()
        }
        fn seen(
            &mut self,
            _handle: oxipresso_engine_api::FileHandle,
            _offset: usize,
            _engine_time: u64,
        ) {
        }
        fn close(&mut self, _handle: oxipresso_engine_api::FileHandle) -> Result<()> {
            Ok(())
        }
        fn picture_bounds_get(
            &mut self,
            _key: &oxipresso_engine_api::PictureKey,
        ) -> Option<[f32; 4]> {
            None
        }
        fn picture_bounds_set(
            &mut self,
            _key: oxipresso_engine_api::PictureKey,
            _bounds: [f32; 4],
        ) {
        }
        fn snapshot_inputs(&mut self) -> Result<Vec<(String, Vec<u8>)>> {
            Ok(self.snapshots.clone())
        }
    }

    #[test]
    fn reads_root_from_engine_io() {
        let root = RootDocument {
            root_dir: env::current_dir().unwrap(),
            root_name: "main.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes: b"abc".to_vec(),
            snapshots: Vec::new(),
        };
        assert_eq!(read_root_bytes(&root, &mut io).unwrap(), b"abc");
    }

    #[test]
    fn compiles_simple_tex_when_xelatex_is_available() {
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let temp = unique_build_dir().unwrap();
        fs::create_dir_all(&temp).unwrap();
        let root = RootDocument {
            root_dir: temp.clone(),
            root_name: "main.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes: b"\\documentclass{article}\n\\begin{document}\nHello\n\\end{document}\n"
                .to_vec(),
            snapshots: Vec::new(),
        };
        let mut engine = ExternalEngine::xelatex();
        engine.initialize(&root, &mut io).unwrap();
        assert_eq!(engine.output_document().unwrap().kind, ArtifactKind::Pdf);
        let synctex = engine.output_synctex().unwrap();
        assert!(!synctex.bytes.is_empty());
        assert!(synctex.source_name.as_deref().unwrap().contains(".synctex"));
        let parsed = oxipresso_synctex::parse_artifact(&synctex).unwrap();
        assert!(
            parsed
                .inputs
                .iter()
                .any(|input| input.path.contains("main.tex"))
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn compiles_original_texpresso_simple_fixture_when_available() {
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("simple.tex") else {
            return;
        };
        let bytes = fs::read(&fixture).unwrap();
        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: fixture.file_name().unwrap().to_string_lossy().to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes,
            snapshots: Vec::new(),
        };
        let mut engine = ExternalEngine::xelatex();
        engine.initialize(&root, &mut io).unwrap();
        let artifact = engine.output_document().unwrap();
        assert_eq!(artifact.kind, ArtifactKind::Pdf);
        assert!(artifact.bytes.starts_with(b"%PDF-"));
    }

    #[test]
    fn texinputs_includes_root_build_and_include_paths() {
        let root = RootDocument {
            root_dir: PathBuf::from("C:/root"),
            root_name: "main.tex".to_string(),
            include_paths: vec![PathBuf::from("inc"), PathBuf::from("D:/shared")],
            stream_mode: false,
        };
        let texinputs = texinputs_for(&root, Path::new("C:/build"));
        assert!(texinputs.contains("C:/root/"));
        assert!(texinputs.contains("C:/build/"));
        assert!(texinputs.contains("C:/root/inc/"));
        assert!(texinputs.contains("D:/shared/"));
        assert!(texinputs.ends_with(if cfg!(windows) { ';' } else { ':' }));
    }

    #[test]
    fn compiles_original_texpresso_include_fixture_with_include_path_when_available() {
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("include.tex") else {
            return;
        };
        let test_dir = fixture.parent().unwrap();
        let bytes = fs::read(&fixture).unwrap();
        let root = RootDocument {
            root_dir: test_dir.to_path_buf(),
            root_name: fixture.file_name().unwrap().to_string_lossy().to_string(),
            include_paths: vec![test_dir.join("incpath")],
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes,
            snapshots: Vec::new(),
        };
        let mut engine = ExternalEngine::xelatex();
        engine.initialize(&root, &mut io).unwrap();
        let artifact = engine.output_document().unwrap();
        assert_eq!(artifact.kind, ArtifactKind::Pdf);
        assert!(artifact.bytes.starts_with(b"%PDF-"));
    }

    #[test]
    fn original_texpresso_missing_input_fixture_reports_diagnostics_when_available() {
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("missing-input.tex")
        else {
            return;
        };
        let bytes = fs::read(&fixture).unwrap();
        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: fixture.file_name().unwrap().to_string_lossy().to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes,
            snapshots: Vec::new(),
        };
        let mut engine = ExternalEngine::xelatex();
        let error = engine.initialize(&root, &mut io).unwrap_err();
        assert!(error.to_string().contains("xelatex failed"));
        let diagnostics = engine
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<String>();
        assert!(diagnostics.contains("texpresso_ci_missing_file"));
        assert!(engine.output_document().is_none());
    }

    #[test]
    fn compiles_original_texpresso_includegraphics_fixture_when_available() {
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("includegraphics.tex")
        else {
            return;
        };
        if !fixture
            .parent()
            .and_then(|test_dir| test_dir.parent())
            .map(|root| root.join("doc").join("texpresso_logo_v2.png").exists())
            .unwrap_or(false)
        {
            return;
        }
        let bytes = fs::read(&fixture).unwrap();
        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: fixture.file_name().unwrap().to_string_lossy().to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes,
            snapshots: Vec::new(),
        };
        let mut engine = ExternalEngine::xelatex();
        engine.initialize(&root, &mut io).unwrap();
        let artifact = engine.output_document().unwrap();
        assert_eq!(artifact.kind, ArtifactKind::Pdf);
        assert!(artifact.bytes.starts_with(b"%PDF-"));
    }

    #[test]
    fn compiles_include_from_engine_io_snapshot_when_xelatex_is_available() {
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let temp = unique_build_dir().unwrap();
        fs::create_dir_all(&temp).unwrap();
        let root = RootDocument {
            root_dir: temp.clone(),
            root_name: "main.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let mut io = MemoryIo {
            bytes:
                b"\\documentclass{article}\n\\begin{document}\n\\input{included}\n\\end{document}\n"
                    .to_vec(),
            snapshots: vec![("included.tex".to_string(), b"from snapshot".to_vec())],
        };
        let mut engine = ExternalEngine::xelatex();
        engine.initialize(&root, &mut io).unwrap();
        let artifact = engine.output_document().unwrap();
        assert_eq!(artifact.kind, ArtifactKind::Pdf);
        assert!(artifact.bytes.starts_with(b"%PDF-"));
        let _ = fs::remove_dir_all(temp);
    }
}
