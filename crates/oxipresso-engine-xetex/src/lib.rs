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
    OxiXetexCallbacks, OxiXetexConfig, OxiXetexResult, oxipresso_xetex_fence_restore_fired,
    oxipresso_xetex_fence_roundtrip_fired, oxipresso_xetex_fence_snapshot_copy,
    oxipresso_xetex_fence_snapshot_len, oxipresso_xetex_is_real,
    oxipresso_xetex_request_fence_park, oxipresso_xetex_request_fence_restore,
    oxipresso_xetex_request_fence_roundtrip, oxipresso_xetex_request_fence_snapshot,
    oxipresso_xetex_run, oxipresso_xetex_snapshot_bytes, oxipresso_xetex_snapshot_capture,
};

/// The C shim keeps global engine state (`active_session`) and is strictly
/// single-instance, so every engine invocation is serialized process-wide.
static ENGINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Controller handle for a parked checkpoint fence (increment b2-loop).
/// The engine thread parks inside its read callback at the fence and blocks
/// on `condvar`; the controller thread observes `parked`, then `submit_edit`s
/// the new bytes for a path, which the parked engine thread applies to its own
/// VFS borrow and answers with restore+replay. One live fence per process
/// (runs are serialized under [`ENGINE_LOCK`]).
#[derive(Debug, Default)]
pub struct FenceControl {
    parked: std::sync::atomic::AtomicBool,
    edit: std::sync::Mutex<Option<(String, Vec<u8>)>>,
    condvar: std::sync::Condvar,
}

impl FenceControl {
    /// Whether the engine thread is currently parked at the fence.
    pub fn is_parked(&self) -> bool {
        self.parked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Spin until the engine parks (or ~30s elapse); returns the parked state.
    pub fn wait_parked(&self) -> bool {
        for _ in 0..3000 {
            if self.is_parked() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.is_parked()
    }

    /// Hand edited bytes to the parked engine (it applies them and asks the
    /// shim to restore the checkpoint and replay the run).
    pub fn submit_edit(&self, path: &str, bytes: Vec<u8>) {
        let mut pending = self
            .edit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = Some((path.to_string(), bytes));
        self.condvar.notify_all();
    }
}

static FENCE_CONTROL: std::sync::Mutex<Option<std::sync::Arc<FenceControl>>> =
    std::sync::Mutex::new(None);

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

    /// Read-only snapshot of the engine's POD memory pools plus the key
    /// cursors (checkpoint increment (a): the capture side of the
    /// setjmp-at-fence design). Header is 12 little-endian u64 words (sizes
    /// and cursors; the two eqtb slots are fixed 0 — eqtb is a segment-pointer
    /// graph, not a POD pool, see the shim comment), then the pool blocks in
    /// fixed order (mem, str_start, str_pool, save_stack, font_info).
    /// The engine frees every pool at run exit, so this only returns data
    /// while a run is active; post-run and stub builds return `None`.
    pub fn pool_snapshot() -> Option<Vec<u8>> {
        if !Self::real_mode() {
            return None;
        }
        let _guard = ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            let need = oxipresso_xetex_snapshot_bytes();
            if need == 0 {
                return None;
            }
            let mut buf = vec![0u8; need as usize];
            let wrote = oxipresso_xetex_snapshot_capture(buf.as_mut_ptr().cast(), need);
            if wrote < 0 {
                return None;
            }
            buf.truncate(wrote as usize);
            Some(buf)
        }
    }

    /// Arm a one-shot at-fence pool capture for the next run (real mode).
    /// The shim captures the live pools the first time the engine reads a
    /// non-format file — its fork-equivalent fence — so the capture happens
    /// while the pools are allocated. Retrieve with [`Self::take_fence_snapshot`].
    pub fn arm_fence_snapshot() {
        if Self::real_mode() {
            unsafe { oxipresso_xetex_request_fence_snapshot() };
        }
    }

    /// Copy out the bytes captured by the last armed fence snapshot, if any
    /// (real mode). Returns `None` in stub builds or when no capture ran.
    pub fn take_fence_snapshot() -> Option<Vec<u8>> {
        if !Self::real_mode() {
            return None;
        }
        let _guard = ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            let len = oxipresso_xetex_fence_snapshot_len();
            if len == 0 {
                return None;
            }
            let mut buf = vec![0u8; len as usize];
            let copied = oxipresso_xetex_fence_snapshot_copy(buf.as_mut_ptr().cast(), len);
            if copied == 0 {
                return None;
            }
            buf.truncate(copied as usize);
            Some(buf)
        }
    }

    /// Arm a live-frame `setjmp`/`longjmp` round-trip at the next fence
    /// (checkpoint increment b1). Like [`Self::arm_fence_snapshot`] it must
    /// be called before the run; the fence captures, then longjmps back into
    /// the same still-live read callback, proving the suspension point works
    /// before increment (b2) ever restores state there.
    pub fn arm_fence_roundtrip() {
        if Self::real_mode() {
            unsafe { oxipresso_xetex_request_fence_roundtrip() };
        }
    }

    /// Whether the armed fence round-trip actually longjmped during the last
    /// run (real mode; `false` in stubs or if the hook never fired).
    pub fn fence_roundtrip_fired() -> bool {
        if !Self::real_mode() {
            return false;
        }
        let _guard = ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { oxipresso_xetex_fence_roundtrip_fired() != 0 }
    }

    /// Arm an identity restore+replay at the next fence (checkpoint
    /// increment b2-mech): the fence captures the pools, writes them back
    /// over the live ones (refusing on any size/header mismatch), and
    /// longjmps so the engine REPLAYS its typesetting from the checkpoint.
    /// Takes precedence over an armed round-trip.
    pub fn arm_fence_restore() {
        if Self::real_mode() {
            unsafe { oxipresso_xetex_request_fence_restore() };
        }
    }

    /// Whether the armed fence restore actually replayed during the last run
    /// (real mode; `false` in stubs or if the path never fired).
    pub fn fence_restore_fired() -> bool {
        if !Self::real_mode() {
            return false;
        }
        let _guard = ENGINE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { oxipresso_xetex_fence_restore_fired() != 0 }
    }

    /// Arm a parkable checkpoint fence for the next run (real mode): the
    /// engine's read callback will park at the fence and call the Rust
    /// controller; run the engine on your worker thread and drive it with the
    /// returned [`FenceControl`] (wait_parked / submit_edit). The replay reads
    /// the edited buffer from the restored checkpoint state.
    pub fn arm_fence_park() -> Option<std::sync::Arc<FenceControl>> {
        if !Self::real_mode() {
            return None;
        }
        let control = std::sync::Arc::new(FenceControl::default());
        *FENCE_CONTROL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(control.clone());
        unsafe { oxipresso_xetex_request_fence_park() };
        Some(control)
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
            build_date: parse_source_date_epoch(std::env::var_os("SOURCE_DATE_EPOCH")),
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
            fence: Some(callback_fence),
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
            state.read_files.insert(path_key(path));
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

/// Checkpoint fence controller callback (increment b2-loop). Runs ON the
/// engine thread while its read is parked: signals `parked`, blocks until the
/// controller submits an edited buffer (bounded 60s so a wedged controller
/// can never hang CI — timeout continues unmodified), applies the edit through
/// its own `&mut EngineIo` borrow (the same legal path every other callback
/// uses), then asks the shim to restore+replay (2) or continue (3).
unsafe extern "C" fn callback_fence(userdata: *mut c_void) -> c_int {
    let control = FENCE_CONTROL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let Some(control) = control else {
        return 3;
    };
    let Some(state) = callback_state(userdata) else {
        return 3;
    };
    control
        .parked
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut pending = control
        .edit
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while pending.is_none() {
        let (guard, wait) = control
            .condvar
            .wait_timeout(pending, std::time::Duration::from_millis(200))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending = guard;
        if wait.timed_out() && std::time::Instant::now() >= deadline {
            break;
        }
    }
    control
        .parked
        .store(false, std::sync::atomic::Ordering::SeqCst);
    match pending.take() {
        Some((path, bytes)) => {
            if state.io.inject_editor(&path, bytes) {
                2
            } else {
                3
            }
        }
        None => 3,
    }
}

/// Canonical key for matching engine-read paths against VFS change-hint paths:
/// forward slashes and no leading `./`. Both sides of the `apply_change_hint`
/// comparison run through this, so an engine path like `sub\inc.tex` still
/// matches the VFS-normalized change hint `sub/inc.tex`. Deliberately minimal
/// (it does not merge relative and absolute forms), so it only ever makes a
/// genuine match succeed.
/// TeXpresso honors the `SOURCE_DATE_EPOCH` environment variable for a
/// reproducible build timestamp (see `main.c`: `strtoll(getenv(...))`). We do
/// the same, but fall back to a deterministic `0` (not the current time) when
/// it is unset/invalid, so warm rebuilds and cached render digests stay stable.
fn parse_source_date_epoch(raw: Option<std::ffi::OsString>) -> u64 {
    raw.and_then(|value| value.into_string().ok())
        .map(|text| text.trim().to_owned())
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0)
}

fn path_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    match normalized.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => normalized,
    }
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
        // Compare on a canonical path key (forward slashes, no leading "./") so
        // an edit to an engine-read file is not wrongly skipped just because
        // the engine's requested path and the VFS-normalized change path use
        // different separators/forms. Canonicalizing only ever makes a genuine
        // match succeed; a false over-match costs an unnecessary (correct)
        // rebuild rather than serving a stale preview.
        if !self.read_files.contains(&path_key(&changed_file.0)) {
            return Ok(RestartPolicy::NoRestartNeeded);
        }
        Ok(RestartPolicy::FullRestartRequired)
    }

    fn restart(&mut self, _io: &mut dyn EngineIo) -> Result<()> {
        // A restart discards the current run's derived state so a consumer
        // inspecting the engine before the next run sees a clean slate rather
        // than a stale sidecar/stream/diagnostics from the prior run.
        self.output = None;
        self.output_synctex = None;
        self.output_events.clear();
        self.diagnostics.clear();
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
    fn change_hint_matches_engine_path_regardless_of_separator() {
        let mut engine = XetexEngine::new();
        // The engine opened an include under a Windows-style backslash path.
        engine.read_files.insert(path_key("sub\\inc.tex"));
        // A VFS-normalized (forward slash) change hint must resolve to a real
        // dependency, not be skipped — the pre-fix exact-string compare said No.
        assert_eq!(
            engine
                .apply_change_hint(&PathId("sub/inc.tex".to_string()), 0)
                .unwrap(),
            RestartPolicy::FullRestartRequired,
            "backslash-read path must match a slash change hint"
        );
        assert_eq!(
            engine
                .apply_change_hint(&PathId("./sub/inc.tex".to_string()), 0)
                .unwrap(),
            RestartPolicy::FullRestartRequired,
            "leading ./ hint must match"
        );
        // Unrelated file still skips, and relative/absolute are NOT conflated.
        assert_eq!(
            engine
                .apply_change_hint(&PathId("other.tex".to_string()), 0)
                .unwrap(),
            RestartPolicy::NoRestartNeeded
        );
        assert_eq!(
            engine
                .apply_change_hint(&PathId("/abs/sub/inc.tex".to_string()), 0)
                .unwrap(),
            RestartPolicy::NoRestartNeeded,
            "must not merge relative and absolute forms"
        );
    }

    #[test]
    fn restart_discards_prior_run_derived_state() {
        if XetexEngine::real_mode() {
            return;
        }
        let mut engine = XetexEngine::new();
        let mut vfs = VirtualFileSystem::new();
        vfs.open_editor("simple.tex", b"hello".to_vec());
        let root = RootDocument {
            root_dir: PathBuf::from("."),
            root_name: "simple.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        engine.initialize(&root, &mut vfs).unwrap();
        assert!(
            engine.output_document().is_some(),
            "stub run yields an artifact"
        );
        assert!(
            !engine.diagnostics().is_empty(),
            "stub run reports diagnostics"
        );

        engine.restart(&mut vfs).unwrap();
        assert!(
            engine.output_document().is_none(),
            "restart must drop the stale artifact"
        );
        assert!(
            engine.diagnostics().is_empty(),
            "restart must clear stale diagnostics"
        );
        assert!(
            engine.output_synctex().is_none(),
            "restart must drop any stale sidecar"
        );
        assert!(
            engine.take_output_events().is_empty(),
            "restart must drop stale output events"
        );
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
        // TeXpresso's always-on `synctex_texpresso_extension` (enabled in the
        // real shim) makes the engine emit `/<tag>` closed-input records; a real
        // TeXpresso sidecar has them, so ours must too (parser tolerates them).
        let synctex_text = std::str::from_utf8(&synctex.bytes).unwrap();
        assert!(
            synctex_text.lines().any(|line| {
                line.len() > 1
                    && line.starts_with('/')
                    && line[1..].bytes().all(|b| b.is_ascii_digit())
            }),
            "syncTeX should carry TeXpresso's /<tag> closed-input records"
        );
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

        // Incremental layer 3 on REAL data: the engine's read-file set gates
        // whether an edit forces a rebuild (edits to files the real engine
        // never opened cost nothing).
        assert_eq!(
            rebuild_engine
                .apply_change_hint(&PathId("simple.tex".to_string()), 0)
                .unwrap(),
            RestartPolicy::FullRestartRequired,
            "editing the primary the real engine read must require a rebuild"
        );
        assert_eq!(
            rebuild_engine
                .apply_change_hint(&PathId("never-read.sty".to_string()), 0)
                .unwrap(),
            RestartPolicy::NoRestartNeeded,
            "editing a file the real engine never opened must not require a rebuild"
        );

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

        // Missing-input robustness on the REAL engine: a document that \inputs
        // an absent file must not yield a successful artifact, and must not
        // wedge the process — the next valid document still typesets (proves
        // halt-on-error + setjmp/longjmp abort capture return control cleanly).
        {
            let mut bad_engine = XetexEngine::new();
            let mut bad_vfs = VirtualFileSystem::new();
            bad_vfs.open_editor(
                "missing-input-test.tex",
                b"\\input{this-file-does-not-exist-oxipresso}\n".to_vec(),
            );
            if let Some(r) = texlive::KpsewhichResolver::auto() {
                bad_vfs.set_resolver(Box::new(r));
            }
            let bad_root = RootDocument {
                root_dir: fixture.parent().unwrap().to_path_buf(),
                root_name: "missing-input-test.tex".to_string(),
                include_paths: Vec::new(),
                stream_mode: false,
            };
            let bad_result = bad_engine.initialize(&bad_root, &mut bad_vfs);
            assert!(
                !(bad_result.is_ok() && bad_engine.output_document().is_some()),
                "a missing \\input must not produce a successful artifact"
            );

            // Recovery: a subsequent valid document still typesets cleanly.
            let mut good_engine = XetexEngine::new();
            let mut good_vfs = VirtualFileSystem::new();
            good_vfs.open_editor("simple.tex", fs::read(&fixture).unwrap());
            good_vfs.set_resolver(Box::new(texlive::KpsewhichResolver::auto().unwrap()));
            good_engine.initialize(&root, &mut good_vfs).unwrap();
            assert!(
                good_engine.output_document().is_some(),
                "the real engine must recover and typeset a valid doc after a failing one"
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

    /// Checkpoint increment (a): the shim captures the live POD pools at the
    /// read fence (mid-run, where TeXpresso forks). Because the engine frees
    /// every pool at run exit, the capture MUST happen during the run — so the
    /// only way to observe it is via the at-fence hook. This proves two things:
    /// (1) the fence capture runs and yields the multi-MB pools, and (2) it is
    /// a pure read — the fence-captured run's XDV is byte-identical to a
    /// control run (build date pinned via `SOURCE_DATE_EPOCH`, so the capture
    /// itself is the only possible perturbation). Increment (b1) adds a third
    /// run: a live-frame `setjmp`/`longjmp` round-trip at the fence, which
    /// must also leave the XDV byte-identical while provably having fired.
    #[test]
    fn real_engine_fence_snapshot_is_readonly_and_output_identical() {
        if env::var("OXIPRESSO_USE_REAL_XETEX").ok().as_deref() != Some("1")
            || env::var_os("TEXPRESSO_SRC").is_none()
            || env::var_os("OXIPRESSO_XETEX_FORMAT").is_none()
        {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("simple.tex") else {
            return;
        };
        // Deterministic engine build date so two identical runs differ only
        // if something (e.g. an at-fence capture) perturbed the engine.
        unsafe { env::set_var("SOURCE_DATE_EPOCH", "1700000000") };

        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: "simple.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let run_and_typeset = |arm: bool| -> Vec<u8> {
            let mut engine = XetexEngine::new();
            let mut vfs = VirtualFileSystem::new();
            vfs.open_editor("simple.tex", fs::read(&fixture).unwrap());
            // Fonts/TFMs come from the TeX distribution.
            vfs.set_resolver(Box::new(
                texlive::KpsewhichResolver::auto().expect("kpsewhich resolver for real-mode test"),
            ));
            if arm {
                XetexEngine::arm_fence_snapshot();
            }
            engine
                .initialize(&root, &mut vfs)
                .expect("real engine initialize");
            engine
                .output_document()
                .expect("real run must produce an artifact")
                .bytes
                .clone()
        };

        let control = run_and_typeset(false);
        let fenced = run_and_typeset(true);
        assert_eq!(
            control, fenced,
            "an at-fence pool capture must not perturb typesetting output"
        );

        let snap = XetexEngine::take_fence_snapshot().expect("fence capture must have run");
        assert!(
            snap.len() > 12 * 8 + (1 << 20),
            "fence snapshot should carry multi-MB engine pools, got {}",
            snap.len()
        );
        let word = |i: usize| u64::from_le_bytes(snap[i * 8..i * 8 + 8].try_into().unwrap());
        assert!(word(0) > 0, "mem block size recorded");
        assert!(word(6) > 0, "mem_end cursor after format load");
        assert!(word(9) > 0, "pool_ptr cursor after format load");
        assert!(word(11) > 0, "fmem_ptr cursor after font load");

        // Increment (b1): arm the live-frame setjmp/longjmp round-trip and
        // re-run. Output must stay byte-identical, AND the round-trip must
        // actually have fired — proving the fence frame survives a longjmp
        // back into it (the mechanism increment (b2) restore will use).
        XetexEngine::arm_fence_roundtrip();
        let roundtrip = run_and_typeset(true);
        assert!(
            XetexEngine::fence_roundtrip_fired(),
            "fence round-trip must have longjmped through the live frame"
        );
        assert!(
            XetexEngine::take_fence_snapshot().is_some(),
            "the round-trip run must also have captured the pools"
        );
        assert_eq!(
            control, roundtrip,
            "a live-frame longjmp at the fence must not perturb typesetting output"
        );

        // Increment (b2-mech): identity restore + replay. The fence captures,
        // memcpy's the snapshot back over the live pools, and longjmps — the
        // engine replays its run from the checkpoint. The completed XDV must
        // be byte-identical to the control, and the restore must have fired.
        XetexEngine::arm_fence_restore();
        let replay = run_and_typeset(true);
        assert!(
            XetexEngine::fence_restore_fired(),
            "fence restore must have replayed the engine from the snapshot"
        );
        assert_eq!(
            control, replay,
            "restore+longjmp replay at the fence must not perturb typesetting output"
        );
        unsafe { env::remove_var("SOURCE_DATE_EPOCH") };
    }

    /// Checkpoint increment (b2-loop): a real rebuild via checkpoint resume.
    /// The engine runs on a worker thread and parks inside its read callback
    /// at the fence; the controller injects an edited root buffer; the engine
    /// restores pool state, REPLAYS from the fence, and now reads the edited
    /// bytes. The result must be byte-identical to a fresh full run over the
    /// edited document — TeXpresso's checkpoint correctness theorem, proven
    /// in-process. The edit both mutates a word and GROWS the file: input EOF
    /// flows through the read bridge (the engine keeps no cached file size —
    /// `ttstub_input_size` has zero call sites in it), so grow/shrink edits
    /// stream through the replay without any size refresh.
    #[test]
    fn real_engine_fence_checkpoint_resume_matches_fresh_edited_run() {
        if env::var("OXIPRESSO_USE_REAL_XETEX").ok().as_deref() != Some("1")
            || env::var_os("TEXPRESSO_SRC").is_none()
            || env::var_os("OXIPRESSO_XETEX_FORMAT").is_none()
        {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("simple.tex") else {
            return;
        };
        unsafe { env::set_var("SOURCE_DATE_EPOCH", "1700000000") };
        let original = fs::read(&fixture).unwrap();
        let mut edited = original.clone();
        let pos = edited
            .windows(6)
            .position(|w| w == b"simple")
            .expect("fixture contains 'simple'");
        edited[pos] = b'S';
        // And grow: append a sentence before the document end.
        let marker = edited
            .windows(14)
            .rposition(|w| w == b"\\end{document}")
            .expect("fixture ends the document env");
        let mut grown = Vec::with_capacity(edited.len() + 40);
        grown.extend_from_slice(&edited[..marker]);
        grown.extend_from_slice(b"An appended checkpoint probe sentence.\n\n");
        grown.extend_from_slice(&edited[marker..]);
        edited = grown;
        let root = RootDocument {
            root_dir: fixture.parent().unwrap().to_path_buf(),
            root_name: "simple.tex".to_string(),
            include_paths: Vec::new(),
            stream_mode: false,
        };
        let run_typeset = |doc: &[u8]| -> Vec<u8> {
            let mut engine = XetexEngine::new();
            let mut vfs = VirtualFileSystem::new();
            vfs.open_editor("simple.tex", doc.to_vec());
            vfs.set_resolver(Box::new(
                texlive::KpsewhichResolver::auto().expect("kpsewhich resolver for real-mode test"),
            ));
            engine
                .initialize(&root, &mut vfs)
                .expect("real engine initialize");
            engine
                .output_document()
                .expect("real run must produce an artifact")
                .bytes
                .clone()
        };
        let baseline = run_typeset(&original);
        let oracle = run_typeset(&edited);
        assert_ne!(
            baseline, oracle,
            "the same-length edit must change the typeset output"
        );

        let control = XetexEngine::arm_fence_park().expect("fence control (real mode)");
        let resumed = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut engine = XetexEngine::new();
                let mut vfs = VirtualFileSystem::new();
                vfs.open_editor("simple.tex", original.clone());
                vfs.set_resolver(Box::new(
                    texlive::KpsewhichResolver::auto()
                        .expect("kpsewhich resolver for real-mode test"),
                ));
                engine
                    .initialize(&root, &mut vfs)
                    .expect("checkpointed initialize");
                engine
                    .output_document()
                    .expect("resumed run must produce an artifact")
                    .bytes
                    .clone()
            });
            assert!(control.wait_parked(), "engine must park at the fence");
            control.submit_edit("simple.tex", edited.clone());
            worker.join().expect("engine worker must not panic")
        });
        assert!(
            XetexEngine::fence_restore_fired(),
            "checkpoint resume must have replayed from the fence"
        );
        assert_eq!(
            oracle, resumed,
            "resume-with-edit must equal a fresh run of the edited document"
        );
        unsafe { env::remove_var("SOURCE_DATE_EPOCH") };
    }

    #[test]
    fn parse_source_date_epoch_matches_texpresso() {
        use std::ffi::OsString;
        // TeXpresso honors SOURCE_DATE_EPOCH (reproducible builds); we parse it
        // and fall back to a deterministic 0 otherwise (never the wall clock).
        assert_eq!(
            parse_source_date_epoch(Some(OsString::from("1700000000"))),
            1700000000
        );
        assert_eq!(parse_source_date_epoch(Some(OsString::from("  42  "))), 42); // trimmed
        assert_eq!(parse_source_date_epoch(None), 0); // unset -> deterministic 0
        assert_eq!(parse_source_date_epoch(Some(OsString::from(""))), 0);
        assert_eq!(
            parse_source_date_epoch(Some(OsString::from("not-a-number"))),
            0
        );
        assert_eq!(parse_source_date_epoch(Some(OsString::from("-5"))), 0); // negative not valid epoch
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
