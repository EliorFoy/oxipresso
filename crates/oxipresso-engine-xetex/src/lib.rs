use std::{
    collections::HashMap,
    env,
    ffi::CString,
    os::raw::{c_char, c_int, c_void},
    path::{Path, PathBuf},
};

pub mod texlive;

use oxipresso_engine_api::{
    ArtifactKind, Diagnostic, DiagnosticSeverity, DocumentArtifact, EngineEvent, EngineInit,
    EngineIo, FileHandle, FileKind, OpenResult, OutputEvent, PathId, RestartPolicy, Result,
    RootDocument, SyncTexArtifact, TypesettingEngine,
};
use oxipresso_engine_xetex_sys::{
    OxiXetexCallbacks, OxiXetexConfig, OxiXetexResult, oxipresso_xetex_is_real, oxipresso_xetex_run,
};

/// The C shim keeps global engine state (`active_session`) and is strictly
/// single-instance, so every engine invocation is serialized process-wide.
static ENGINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How the engine should be driven for one `oxipresso_xetex_run` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineRun {
    /// Typeset the root document from the persisted format file.
    Normal,
    /// Build the format file in INI mode from the format source document
    /// (e.g. `xelatex.ini`), mirroring the original engine bootstrap.
    FormatBootstrap,
}

#[derive(Debug, Default)]
pub struct XetexEngine {
    diagnostics: Vec<Diagnostic>,
    output: Option<DocumentArtifact>,
    output_synctex: Option<SyncTexArtifact>,
    output_events: Vec<OutputEvent>,
    initialized: bool,
    /// Normalized paths of all files the engine opened for reading during the
    /// last run. Used by `apply_change_hint` to skip rebuilds for changes to
    /// files the engine never touched.
    read_files: std::collections::HashSet<String>,
}

impl XetexEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the linked C shim is the real TeXpresso/XeTeX engine
    /// (opt-in `OXIPRESSO_USE_REAL_XETEX=1` build) or the portable stub.
    pub fn real_mode() -> bool {
        unsafe { oxipresso_xetex_is_real() == 1 }
    }

    fn format_path_text() -> String {
        env::var("OXIPRESSO_XETEX_FORMAT").unwrap_or_else(|_| "texpresso.fmt".to_string())
    }

    fn format_source_name() -> String {
        env::var("OXIPRESSO_XETEX_FORMAT_SOURCE").unwrap_or_else(|_| "xelatex.ini".to_string())
    }

    /// Points fontconfig at a usable `fonts.conf` before the engine's font
    /// manager initializes; without it fontconfig prints "Cannot load default
    /// config file" and native system-font lookup degrades. Discovery order:
    /// existing `FONTCONFIG_PATH`, then `OXIPRESSO_FONTCONFIG_PATH`, then the
    /// vcpkg tree (when `VCPKG_ROOT` is set, matching the real-mode dependency
    /// triplet).
    fn prepare_fontconfig_env() {
        if env::var_os("FONTCONFIG_PATH").is_some() {
            return;
        }
        let candidate = env::var_os("OXIPRESSO_FONTCONFIG_PATH").map(PathBuf::from);
        let candidate = candidate.or_else(|| {
            let vcpkg_root = env::var_os("VCPKG_ROOT")?;
            let candidate = PathBuf::from(vcpkg_root)
                .join("installed")
                .join("x64-windows-static-md")
                .join("etc")
                .join("fonts");
            candidate.is_dir().then_some(candidate)
        });
        if let Some(path) = candidate {
            // Edition-2024 unsafe: process-global environment adjustment that
            // must happen before the engine's fontconfig init. `set_process_env`
            // also mirrors into the Windows CRT block so the engine's C fontconfig
            // `getenv` sees it.
            unsafe {
                oxipresso_platform::set_process_env("FONTCONFIG_PATH", path.as_os_str());
            }
        }
    }

    fn run_engine<'a>(
        &mut self,
        root: &RootDocument,
        io: &'a mut dyn EngineIo,
        run: EngineRun,
    ) -> Result<(OxiXetexResult, CallbackState<'a>)> {
        // Serialize engine runs: the C shim's global session state is not
        // reentrant (parallel tests would otherwise race on it).
        let _engine_guard = ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if Self::real_mode() {
            Self::prepare_fontconfig_env();
        }
        let root_dir_text = root.root_dir.to_string_lossy();
        let root_dir = CString::new(root_dir_text.as_bytes())
            .map_err(|_| oxipresso_engine_api::EngineError::new("root dir contains NUL byte"))?;
        let root_name = CString::new(root.root_name.as_bytes())
            .map_err(|_| oxipresso_engine_api::EngineError::new("root name contains NUL byte"))?;
        let format_path_text = Self::format_path_text();
        let format_path = CString::new(format_path_text.as_bytes()).map_err(|_| {
            oxipresso_engine_api::EngineError::new("XeTeX format path contains NUL byte")
        })?;
        let primary_name_text = match run {
            EngineRun::Normal => String::new(),
            EngineRun::FormatBootstrap => Self::format_source_name(),
        };
        let primary_name = CString::new(primary_name_text.as_bytes()).map_err(|_| {
            oxipresso_engine_api::EngineError::new("primary name contains NUL byte")
        })?;
        let config = OxiXetexConfig {
            root_dir: root_dir.as_ptr(),
            root_dir_len: root_dir_text.len(),
            root_name: root_name.as_ptr(),
            root_name_len: root.root_name.len(),
            format_path: format_path.as_ptr(),
            format_path_len: format_path_text.len(),
            primary_name: primary_name.as_ptr(),
            primary_name_len: primary_name_text.len(),
            build_date: 0,
            stream_mode: i32::from(root.stream_mode),
            in_initex_mode: i32::from(run == EngineRun::FormatBootstrap),
            synctex_enabled: i32::from(run == EngineRun::Normal),
        };
        let mut callback_state = CallbackState {
            io,
            read_buffer: Vec::new(),
            last_error: None,
            output_paths: HashMap::new(),
            output_bytes: HashMap::new(),
            output_events: Vec::new(),
            diagnostics: Vec::new(),
            read_files: std::collections::HashSet::new(),
        };
        let callbacks = OxiXetexCallbacks {
            userdata: (&mut callback_state as *mut CallbackState<'_>).cast::<c_void>(),
            open_read: Some(callback_open_read),
            open_write: Some(callback_open_write),
            read: Some(callback_read),
            append: Some(callback_append),
            size: Some(callback_size),
            seen: Some(callback_seen),
            flush: Some(callback_flush),
            close: Some(callback_close),
            diagnostic: Some(callback_diagnostic),
        };
        let mut result = OxiXetexResult::default();
        let status = unsafe { oxipresso_xetex_run(&config, &callbacks, &mut result) };
        if status != 0 {
            return Err(oxipresso_engine_api::EngineError::new(format!(
                "XeTeX FFI backend failed with status {status}"
            )));
        }
        if let Some(error) = callback_state.last_error.clone() {
            self.diagnostics = std::mem::take(&mut callback_state.diagnostics);
            return Err(oxipresso_engine_api::EngineError::new(error));
        }
        Ok((result, callback_state))
    }

    fn run_once(&mut self, root: &RootDocument, io: &mut dyn EngineIo) -> Result<OxiXetexResult> {
        let (result, mut state) = self.run_engine(root, io, EngineRun::Normal)?;
        self.diagnostics = std::mem::take(&mut state.diagnostics);
        self.output = select_output_artifact(&root.root_name, &state.output_bytes);
        self.output_synctex = Self::select_synctex_artifact(&state.output_bytes);
        self.output_events = std::mem::take(&mut state.output_events);
        self.read_files = std::mem::take(&mut state.read_files);
        Ok(result)
    }

    /// Extracts the plain-text SyncTeX sidecar written by the engine
    /// (`<jobname>.synctex`, captured through the output mirror).
    fn select_synctex_artifact(output_bytes: &HashMap<String, Vec<u8>>) -> Option<SyncTexArtifact> {
        let (source_name, bytes) = output_bytes.iter().find(|(path, bytes)| {
            let normalized = path.replace('\\', "/");
            let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
            (file_name.ends_with(".synctex") || file_name.ends_with(".synctex.gz"))
                && !bytes.is_empty()
        })?;
        Some(SyncTexArtifact {
            bytes: bytes.clone(),
            compressed: source_name.ends_with(".gz"),
            source_name: Some(source_name.clone()),
        })
    }

    /// Builds the TeX format file in INI mode when it is missing on disk,
    /// mirroring the original engine `bootstrap_format` flow: run the engine
    /// with the format source (default `xelatex.ini`) as the primary input and
    /// persist the produced format dump. Bootstrap is only attempted by the
    /// real engine; the stub has no INI mode.
    fn bootstrap_format(
        &mut self,
        root: &RootDocument,
        io: &mut dyn EngineIo,
        format_path: &str,
    ) -> Result<()> {
        let (result, state) = self.run_engine(root, io, EngineRun::FormatBootstrap)?;
        let format_bytes = state
            .output_bytes
            .iter()
            .find(|(path, bytes)| {
                let normalized = path.replace('\\', "/");
                let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
                file_name.ends_with(".fmt") && !bytes.is_empty()
            })
            .map(|(_, bytes)| bytes.clone());
        let spotless = result.status == 0;
        match (format_bytes, spotless) {
            (Some(bytes), true) => {
                if let Some(parent) = Path::new(format_path).parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        oxipresso_engine_api::EngineError::new(format!(
                            "failed to create format directory: {error}"
                        ))
                    })?;
                }
                std::fs::write(format_path, bytes).map_err(|error| {
                    oxipresso_engine_api::EngineError::new(format!(
                        "failed to persist format file: {error}"
                    ))
                })?;
                Ok(())
            }
            (Some(_), false) => {
                self.diagnostics = state.diagnostics;
                Err(oxipresso_engine_api::EngineError::new(format!(
                    "real XeTeX format bootstrap finished with engine history status {}",
                    result.status
                )))
            }
            (None, _) => {
                self.diagnostics = state.diagnostics;
                Err(oxipresso_engine_api::EngineError::new(
                    "real XeTeX format bootstrap did not produce a format file",
                ))
            }
        }
    }
}

struct CallbackState<'a> {
    io: &'a mut dyn EngineIo,
    read_buffer: Vec<u8>,
    last_error: Option<String>,
    output_paths: HashMap<u32, String>,
    output_bytes: HashMap<String, Vec<u8>>,
    output_events: Vec<OutputEvent>,
    diagnostics: Vec<Diagnostic>,
    /// Paths of all files opened for reading (normalized).
    read_files: std::collections::HashSet<String>,
}

impl CallbackState<'_> {
    fn set_error(&mut self, error: impl ToString) -> c_int {
        self.last_error = Some(error.to_string());
        -1
    }
}

unsafe extern "C" fn callback_open_read(
    userdata: *mut c_void,
    path: *const c_char,
    path_len: usize,
    kind: c_int,
    handle: *mut u32,
) -> c_int {
    let Some(state) = callback_state(userdata) else {
        return -1;
    };
    let Some(path) = callback_path(path, path_len) else {
        return state.set_error("FFI open_read path is not valid UTF-8");
    };
    match state.io.open_read(path, callback_file_kind(kind)) {
        Ok(OpenResult::Opened {
            handle: file_handle,
            ..
        }) => {
            if handle.is_null() {
                return state.set_error("FFI open_read handle output is null");
            }
            state.read_files.insert(path.to_string());
            unsafe {
                *handle = file_handle.0;
            }
            0
        }
        Ok(OpenResult::Missing) => 1,
        Ok(OpenResult::Promised) => 2,
        Err(error) => state.set_error(error),
    }
}

unsafe extern "C" fn callback_open_write(
    userdata: *mut c_void,
    path: *const c_char,
    path_len: usize,
    kind: c_int,
    handle: *mut u32,
) -> c_int {
    let Some(state) = callback_state(userdata) else {
        return -1;
    };
    let Some(path) = callback_path(path, path_len) else {
        return state.set_error("FFI open_write path is not valid UTF-8");
    };
    match state.io.open_write(path, callback_file_kind(kind)) {
        Ok(file_handle) => {
            if handle.is_null() {
                return state.set_error("FFI open_write handle output is null");
            }
            state.output_paths.insert(file_handle.0, path.to_string());
            state.output_bytes.insert(path.to_string(), Vec::new());
            unsafe {
                *handle = file_handle.0;
            }
            0
        }
        Err(error) => state.set_error(error),
    }
}

unsafe extern "C" fn callback_read(
    userdata: *mut c_void,
    handle: u32,
    offset: usize,
    len: usize,
    bytes: *mut *const u8,
    out_len: *mut usize,
) -> c_int {
    let Some(state) = callback_state(userdata) else {
        return -1;
    };
    if bytes.is_null() || out_len.is_null() {
        return state.set_error("FFI read output pointer is null");
    }
    match state.io.read(FileHandle(handle), offset, len) {
        Ok(data) => {
            state.read_buffer = data;
            unsafe {
                *bytes = state.read_buffer.as_ptr();
                *out_len = state.read_buffer.len();
            }
            0
        }
        Err(error) => state.set_error(error),
    }
}

unsafe extern "C" fn callback_size(userdata: *mut c_void, handle: u32, size: *mut usize) -> c_int {
    let Some(state) = callback_state(userdata) else {
        return -1;
    };
    if size.is_null() {
        return state.set_error("FFI size output pointer is null");
    }
    match state.io.size(FileHandle(handle)) {
        Ok(value) => {
            unsafe {
                *size = value;
            }
            0
        }
        Err(error) => state.set_error(error),
    }
}

unsafe extern "C" fn callback_append(
    userdata: *mut c_void,
    handle: u32,
    bytes: *const u8,
    len: usize,
) -> c_int {
    let Some(state) = callback_state(userdata) else {
        return -1;
    };
    let data = if len == 0 {
        &[]
    } else {
        if bytes.is_null() {
            return state.set_error("FFI append input pointer is null");
        }
        unsafe { std::slice::from_raw_parts(bytes, len) }
    };
    if let Some(path) = state.output_paths.get(&handle).cloned()
        && let Some(output) = state.output_bytes.get_mut(&path)
    {
        let offset = output.len();
        state.output_events.push(OutputEvent {
            path: path.clone(),
            offset,
            data: data.to_vec(),
        });
        output.extend_from_slice(data);
    }
    match state.io.append(FileHandle(handle), data) {
        Ok(()) => 0,
        Err(error) => state.set_error(error),
    }
}

unsafe extern "C" fn callback_seen(
    userdata: *mut c_void,
    handle: u32,
    offset: usize,
    engine_time: u64,
) {
    let Some(state) = callback_state(userdata) else {
        return;
    };
    state.io.seen(FileHandle(handle), offset, engine_time);
}

unsafe extern "C" fn callback_flush(userdata: *mut c_void, _handle: u32) -> c_int {
    let Some(_state) = callback_state(userdata) else {
        return -1;
    };
    0
}

unsafe extern "C" fn callback_close(userdata: *mut c_void, handle: u32) -> c_int {
    let Some(state) = callback_state(userdata) else {
        return -1;
    };
    match state.io.close(FileHandle(handle)) {
        Ok(()) => 0,
        Err(error) => state.set_error(error),
    }
}

unsafe extern "C" fn callback_diagnostic(
    userdata: *mut c_void,
    severity: c_int,
    bytes: *const u8,
    len: usize,
) {
    let Some(state) = callback_state(userdata) else {
        return;
    };
    let data = if len == 0 {
        &[]
    } else if bytes.is_null() {
        state.last_error = Some("FFI diagnostic input pointer is null".to_string());
        return;
    } else {
        unsafe { std::slice::from_raw_parts(bytes, len) }
    };
    state.diagnostics.push(Diagnostic {
        severity: match severity {
            2 => DiagnosticSeverity::Error,
            1 => DiagnosticSeverity::Warning,
            _ => DiagnosticSeverity::Info,
        },
        message: String::from_utf8_lossy(data).to_string(),
        path: None,
        line: None,
    });
}

fn callback_file_kind(kind: c_int) -> FileKind {
    match kind {
        1 => FileKind::Pk,
        3 => FileKind::Tfm,
        4 => FileKind::Afm,
        6 => FileKind::Bib,
        7 => FileKind::Bst,
        8 => FileKind::Cnf,
        10 => FileKind::Format,
        11 => FileKind::FontMap,
        20 => FileKind::Ofm,
        23 => FileKind::Ovf,
        25 => FileKind::Picture,
        26 => FileKind::Tex,
        30 => FileKind::TexPsHeader,
        32 => FileKind::Type1,
        33 => FileKind::Vf,
        36 => FileKind::TrueType,
        39 => FileKind::ProgramData,
        41 => FileKind::MiscFonts,
        44 => FileKind::Enc,
        45 => FileKind::Cmap,
        46 => FileKind::Sfd,
        47 => FileKind::OpenType,
        59 => FileKind::Primary,
        _ => FileKind::Other,
    }
}

fn callback_path<'a>(path: *const c_char, len: usize) -> Option<&'a str> {
    if path.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(path.cast::<u8>(), len) };
    std::str::from_utf8(bytes).ok()
}

fn callback_state<'a>(userdata: *mut c_void) -> Option<&'a mut CallbackState<'a>> {
    if userdata.is_null() {
        return None;
    }
    Some(unsafe { &mut *userdata.cast::<CallbackState<'a>>() })
}

fn select_output_artifact(
    root_name: &str,
    output_bytes: &HashMap<String, Vec<u8>>,
) -> Option<DocumentArtifact> {
    let stem = Path::new(root_name).file_stem()?.to_string_lossy();
    let candidates = [
        (format!("{stem}.xdv"), ArtifactKind::Xdv),
        (format!("{stem}.pdf"), ArtifactKind::Pdf),
        (format!("{stem}.dvi"), ArtifactKind::Dvi),
    ];
    for (candidate, kind) in candidates {
        if let Some((source_name, bytes)) = output_bytes
            .iter()
            .find(|(path, _)| output_name_matches(path, &candidate))
            && !bytes.is_empty()
        {
            return Some(DocumentArtifact {
                kind,
                bytes: bytes.clone(),
                source_name: Some(source_name.clone()),
            });
        }
    }
    None
}

fn output_name_matches(path: &str, file_name: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized == file_name || normalized.ends_with(&format!("/{file_name}"))
}

impl TypesettingEngine for XetexEngine {
    fn initialize(&mut self, root: &RootDocument, io: &mut dyn EngineIo) -> Result<EngineInit> {
        let format_path = Self::format_path_text();
        if Self::real_mode() && !Path::new(&format_path).is_file() {
            self.bootstrap_format(root, io, &format_path)?;
        }
        self.run_once(root, io)?;
        self.initialized = true;
        Ok(EngineInit {
            engine_name: if Self::real_mode() {
                "xetex-real"
            } else {
                "xetex-ffi-stub"
            }
            .to_string(),
        })
    }

    fn step(&mut self, _io: &mut dyn EngineIo) -> Result<EngineEvent> {
        Ok(if self.initialized {
            EngineEvent::Terminated
        } else {
            EngineEvent::Idle
        })
    }

    fn apply_change_hint(
        &mut self,
        changed_file: &PathId,
        _byte_offset: usize,
    ) -> Result<RestartPolicy> {
        // If the engine never opened the changed file during the last run,
        // the current output doesn't depend on it and the rebuild can be
        // skipped (the engine will pick up the change on the next rebuild).
        if !self.read_files.contains(&changed_file.0) {
            return Ok(RestartPolicy::NoRestartNeeded);
        }
        Ok(RestartPolicy::FullRestartRequired)
    }

    fn restart(&mut self, _io: &mut dyn EngineIo) -> Result<()> {
        self.output = None;
        Ok(())
    }

    fn output_document(&self) -> Option<DocumentArtifact> {
        self.output.clone()
    }

    fn output_synctex(&self) -> Option<SyncTexArtifact> {
        self.output_synctex.clone()
    }

    fn take_output_events(&mut self) -> Vec<OutputEvent> {
        std::mem::take(&mut self.output_events)
    }

    fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxipresso_vfs::VirtualFileSystem;
    use std::{env, fs, path::PathBuf};

    #[test]
    fn ffi_stub_initializes() {
        if XetexEngine::real_mode() {
            // Stub-behavior assertions do not hold when the real engine shim
            // is linked in; real-mode flows are covered by the opt-in tests.
            return;
        }
        let mut engine = XetexEngine::new();
        let mut vfs = VirtualFileSystem::new();
        let init = engine
            .initialize(
                &RootDocument {
                    root_dir: PathBuf::from("."),
                    root_name: "simple.tex".to_string(),
                    include_paths: Vec::new(),
                    stream_mode: false,
                },
                &mut vfs,
            )
            .unwrap();
        assert_eq!(init.engine_name, "xetex-ffi-stub");
        assert!(
            engine
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("FFI stub initialized"))
        );
    }

    #[test]
    fn ffi_stub_uses_engine_io_callbacks() {
        if XetexEngine::real_mode() {
            return;
        }
        let mut engine = XetexEngine::new();
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("simple.tex", b"hello".to_vec());
        engine
            .initialize(
                &RootDocument {
                    root_dir: PathBuf::from("."),
                    root_name: "simple.tex".to_string(),
                    include_paths: Vec::new(),
                    stream_mode: false,
                },
                &mut vfs,
            )
            .unwrap();

        assert_eq!(vfs.lookup("simple.tex").unwrap().seen_offset, Some(5));
        assert_eq!(
            vfs.lookup("stdout").unwrap().saved_output.as_slice(),
            b"Oxipresso XeTeX FFI stub\n"
        );
        assert_eq!(
            vfs.lookup("simple.xdv").unwrap().saved_output.as_slice(),
            b"OXIPRESSO-STUB-XDV"
        );
        let artifact = engine.output_document().unwrap();
        assert_eq!(artifact.kind, ArtifactKind::Xdv);
        assert_eq!(artifact.bytes, b"OXIPRESSO-STUB-XDV");
        assert_eq!(artifact.source_name.as_deref(), Some("simple.xdv"));
    }

    #[test]
    fn real_xetex_bootstrap_builds_format_and_typesets_simple() {
        if !XetexEngine::real_mode() {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("simple.tex") else {
            return;
        };
        let Some(resolver) = texlive::KpsewhichResolver::auto() else {
            return;
        };
        let format_path = env::temp_dir().join("oxipresso-bootstrap-texpresso.fmt");
        let _ = fs::remove_file(&format_path);
        // Edition-2024 unsafe: process-global env used to point the engine at
        // this test's format file. Test binaries run tests in parallel, but no
        // other test in this binary depends on the value.
        unsafe {
            env::set_var("OXIPRESSO_XETEX_FORMAT", &format_path);
        }

        let mut engine = XetexEngine::new();
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("simple.tex", fs::read(&fixture).unwrap());
        vfs.set_resolver(Box::new(resolver));
        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: "simple.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };

        let init = engine
            .initialize(&root, &mut vfs)
            .expect("bootstrap + typeset should succeed");
        assert_eq!(init.engine_name, "xetex-real");

        let format_bytes = fs::read(&format_path)
            .expect("bootstrap should persist a format file at OXIPRESSO_XETEX_FORMAT");
        assert!(!format_bytes.is_empty());

        let artifact = engine
            .output_document()
            .expect("real engine should produce an artifact for simple.tex");
        assert!(!artifact.bytes.is_empty());
        assert!(matches!(
            artifact.kind,
            ArtifactKind::Xdv | ArtifactKind::Pdf | ArtifactKind::Dvi
        ));

        // A second initialization (rebuild) must reuse the persisted format.
        let mut rebuild_engine = XetexEngine::new();
        let mut rebuild_vfs = VirtualFileSystem::new();
        rebuild_vfs.open_editor("simple.tex", fs::read(&fixture).unwrap());
        rebuild_vfs.set_resolver(Box::new(texlive::KpsewhichResolver::auto().unwrap()));
        rebuild_engine.initialize(&root, &mut rebuild_vfs).unwrap();
        assert!(rebuild_engine.output_document().is_some());

        // SyncTeX sidecar: the real engine writes a plain-text .synctex that
        // the Rust mirror captures and the synctex parser can read.
        let synctex = rebuild_engine.output_synctex().expect("syncTeX sidecar");
        assert!(!synctex.compressed);
        assert!(synctex.bytes.starts_with(b"SyncTeX Version:"));
        let document = oxipresso_synctex::parse_artifact(&synctex).unwrap();
        assert!(
            !document.inputs.is_empty(),
            "syncTeX should record at least the primary input"
        );
        // Forward search: a line inside the document body maps to a page hit.
        assert!(
            document.forward_search_path("simple.tex", 4).is_some(),
            "syncTeX forward search should find a hit for simple.tex line 4"
        );

        // Reverse search: querying at a real record's own coordinates returns
        // a hit bound to a recorded input file (exact point => distance 0, so
        // this is content-independent and not brittle).
        {
            let record = document
                .records
                .iter()
                .find(|record| document.input_by_index(record.input_index).is_some())
                .expect("real syncTeX should contain a record with a known input");
            let hit = document
                .reverse_search_page_point(record.page, record.x, record.y)
                .expect("reverse search at a record's own point must hit");
            assert!(
                !hit.path.is_empty(),
                "reverse hit should carry the source path, got {hit:?}"
            );
            assert_eq!(hit.input_index, record.input_index);
        }

        // Output streams: stdout chunks feed the `out` buffer and the .log
        // file feeds the `log` buffer, in write order.
        let events = rebuild_engine.take_output_events();
        assert!(
            events.iter().any(|event| event.path == "stdout"),
            "engine stdout should be captured as output events"
        );
        assert!(
            events
                .iter()
                .any(|event| event.path.replace('\\', "/").ends_with(".log")),
            "engine log file should be captured as output events"
        );
        let stdout_text = events
            .iter()
            .filter(|event| event.path == "stdout")
            .map(|event| String::from_utf8_lossy(&event.data))
            .collect::<String>();
        assert!(
            stdout_text.to_lowercase().contains("xetex"),
            "engine stdout should contain the XeTeX banner, got {stdout_text:?}"
        );

        // Original `include.tex` fixture through the REAL engine, with the
        // `incpath` directory on the search path so `\input{test.tex}` resolves.
        {
            let Some(include) = oxipresso_testkit::original_texpresso_fixture("include.tex") else {
                return;
            };
            let Some(resolver) = texlive::KpsewhichResolver::auto() else {
                return;
            };
            let include_dir = include.parent().unwrap().to_path_buf();
            let incpath = include_dir.join("incpath");
            let mut engine = XetexEngine::new();
            let mut vfs = VirtualFileSystem::new();
            vfs.open_editor("include.tex", fs::read(&include).unwrap());
            vfs.set_disk_roots(vec![include_dir.clone(), incpath.clone()]);
            vfs.set_resolver(Box::new(resolver));
            let root = RootDocument {
                root_dir: include_dir,
                root_name: "include.tex".to_string(),
                include_paths: vec![incpath],
                stream_mode: false,
            };
            engine
                .initialize(&root, &mut vfs)
                .expect("real engine should typeset include.tex");
            let artifact = engine
                .output_document()
                .expect("include.tex should produce an artifact");
            assert!(!artifact.bytes.is_empty());
            let inputs = vfs.take_input_events();
            assert!(
                inputs
                    .iter()
                    .any(|event| event.path.replace('\\', "/").ends_with("test.tex")),
                "`\\input{{test.tex}}` should read incpath/test.tex through -I, inputs: {inputs:?}"
            );
        }

        // Original `includegraphics.tex` fixture: the PNG resolves through the
        // VFS and the engine embeds it as a `pdf:image` special in the XDV.
        {
            let Some(graphics) =
                oxipresso_testkit::original_texpresso_fixture("includegraphics.tex")
            else {
                return;
            };
            let Some(resolver) = texlive::KpsewhichResolver::auto() else {
                return;
            };
            let graphics_dir = graphics.parent().unwrap().to_path_buf();
            let mut engine = XetexEngine::new();
            let mut vfs = VirtualFileSystem::new();
            vfs.open_editor("includegraphics.tex", fs::read(&graphics).unwrap());
            vfs.set_disk_roots(vec![graphics_dir.clone()]);
            vfs.set_resolver(Box::new(resolver));
            let root = RootDocument {
                root_dir: graphics_dir,
                root_name: "includegraphics.tex".to_string(),
                include_paths: Vec::new(),
                stream_mode: false,
            };
            engine
                .initialize(&root, &mut vfs)
                .expect("real engine should typeset includegraphics.tex");
            let artifact = engine
                .output_document()
                .expect("includegraphics.tex should produce an artifact");
            assert!(!artifact.bytes.is_empty());
            let xdv_text = String::from_utf8_lossy(&artifact.bytes);
            assert!(
                xdv_text.contains("pdf:image"),
                "XDV should embed a `pdf:image` special for the included graphic"
            );
            assert!(
                xdv_text.contains("texpresso_logo_v2.png"),
                "the `pdf:image` special should name the logo PNG"
            );
            let inputs = vfs.take_input_events();
            assert!(
                inputs.iter().any(|event| event
                    .path
                    .replace('\\', "/")
                    .ends_with("texpresso_logo_v2.png")),
                "the logo PNG should be resolved through the VFS, inputs: {inputs:?}"
            );
        }
    }

    #[test]
    fn real_xetex_smoke_when_enabled() {
        if env::var("OXIPRESSO_USE_REAL_XETEX").ok().as_deref() != Some("1")
            || env::var_os("TEXPRESSO_SRC").is_none()
            || env::var_os("OXIPRESSO_XETEX_FORMAT").is_none()
        {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("simple.tex") else {
            return;
        };
        let mut engine = XetexEngine::new();
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("simple.tex", fs::read(&fixture).unwrap());
        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: "simple.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let _ = engine.initialize(&root, &mut vfs);
        assert!(
            !engine.diagnostics().is_empty() || engine.output_document().is_some(),
            "real XeTeX FFI smoke should emit diagnostics or an artifact"
        );
        if let Some(artifact) = engine.output_document() {
            match artifact.kind {
                ArtifactKind::Pdf => assert!(artifact.bytes.starts_with(b"%PDF-")),
                ArtifactKind::Xdv | ArtifactKind::Dvi => assert!(!artifact.bytes.is_empty()),
                ArtifactKind::Unknown => panic!("real XeTeX smoke produced unknown artifact kind"),
            }
        }
    }

    #[test]
    fn maps_tectonic_file_formats_to_engine_file_kinds() {
        assert_eq!(callback_file_kind(10), FileKind::Format);
        assert_eq!(callback_file_kind(25), FileKind::Picture);
        assert_eq!(callback_file_kind(26), FileKind::Tex);
        assert_eq!(callback_file_kind(36), FileKind::TrueType);
        assert_eq!(callback_file_kind(47), FileKind::OpenType);
        assert_eq!(callback_file_kind(59), FileKind::Primary);
        assert_eq!(callback_file_kind(12345), FileKind::Other);
    }
}
