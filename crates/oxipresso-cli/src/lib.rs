use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use oxipresso_editor_protocol::{
    EditorCommand, EditorMessage, InfoBuffer, WireProtocol, parse_command, serialize_message,
};
use oxipresso_engine_api::{EngineIo, OutputEvent, RestartPolicy, RootDocument, TypesettingEngine};
use oxipresso_engine_external::ExternalEngine;
use oxipresso_engine_xetex::XetexEngine;
use oxipresso_render::AutoRenderBackend;
#[cfg(feature = "freetype")]
use oxipresso_render::{
    FontResolver as GlyphFontResolver, ImageLoader as GlyphImageLoader, XdvGlyphRenderBackend,
};
use oxipresso_synctex::SyncTexDocument;
use oxipresso_vfs::{ChangeOutcome, VirtualFileSystem};
use oxipresso_viewer::{ViewerState, ViewerSyncPosition};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageProvider {
    Auto,
    Texlive,
    Tectonic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOptions {
    pub include_paths: Vec<PathBuf>,
    pub protocol: WireProtocol,
    pub line_output: bool,
    pub provider: PackageProvider,
    pub initialize_only: bool,
    pub stream_mode: bool,
    /// Run the engine and an egui live-preview window in this process, with
    /// the editor wire still on stdin/stdout (the TeXpresso architecture).
    pub gui: bool,
    pub root_file: PathBuf,
}

pub fn parse_args<I, S>(args: I) -> Result<CliOptions, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut include_paths = Vec::new();
    let mut protocol = WireProtocol::Sexp;
    let mut line_output = false;
    let mut provider = PackageProvider::Auto;
    let mut initialize_only = false;
    let mut stream_mode = false;
    let mut gui = false;
    let mut root_file = None;
    let mut iter = args.into_iter().map(Into::into);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-I" => {
                let Some(path) = iter.next() else {
                    return Err("-I expects a path".to_string());
                };
                include_paths.push(PathBuf::from(path));
            }
            "-json" => protocol = WireProtocol::Json,
            "-lines" => line_output = true,
            "-texlive" => {
                if provider == PackageProvider::Tectonic {
                    return Err("-texlive and -tectonic are mutually exclusive".to_string());
                }
                provider = PackageProvider::Texlive;
            }
            "-tectonic" => {
                if provider == PackageProvider::Texlive {
                    return Err("-texlive and -tectonic are mutually exclusive".to_string());
                }
                provider = PackageProvider::Tectonic;
            }
            "-test-initialize" => initialize_only = true,
            "-stream" => stream_mode = true,
            "-gui" => gui = true,
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ => {
                if root_file.replace(PathBuf::from(&arg)).is_some() {
                    return Err("expected a single root TeX document".to_string());
                }
            }
        }
    }
    Ok(CliOptions {
        include_paths,
        protocol,
        line_output,
        provider,
        initialize_only,
        stream_mode,
        gui,
        root_file: root_file.ok_or_else(|| "missing root TeX document".to_string())?,
    })
}

#[cfg(feature = "gui")]
pub mod gui;

#[cfg(feature = "gui")]
pub use gui::run_live_preview;

pub mod editor_wire;

pub fn run_with_io<R, W>(options: CliOptions, input: R, mut output: W) -> Result<(), String>
where
    R: BufRead,
    W: Write,
{
    let root = root_document(&options)?;
    // The Tectonic provider's bundle environment is validated up front so a
    // misconfigured `-tectonic` run fails loudly instead of silently falling
    // back to TeX Live resolution.
    if options.provider == PackageProvider::Tectonic {
        oxipresso_engine_xetex::tectonic::DirBundleResolver::check_env()
            .map_err(|e| format!("-tectonic: {e}"))?;
    }
    let mut app = OxipressoApp::new(options, root);
    for message in app.initialize()? {
        writeln!(
            output,
            "{}",
            serialize_message(&message, app.options.protocol)
        )
        .map_err(|e| e.to_string())?;
    }
    for line in input.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        for message in app.handle_editor_line(&line)? {
            writeln!(
                output,
                "{}",
                serialize_message(&message, app.options.protocol)
            )
            .map_err(|e| e.to_string())?;
        }
        if app.options.initialize_only && !app.paused {
            break;
        }
    }
    Ok(())
}

/// Canonical key for matching changed paths against the editor input set:
/// forward slashes, no leading `./` (same normalization the engine's
/// change-hint path uses).
fn canonical_change_key(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    match normalized.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => normalized,
    }
}

fn root_document(options: &CliOptions) -> Result<RootDocument, String> {
    let root_file = if options.root_file.is_absolute() {
        options.root_file.clone()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(&options.root_file)
    };
    let root_dir = root_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let root_name = root_file
        .file_name()
        .ok_or_else(|| "root path has no file name".to_string())?
        .to_string_lossy()
        .to_string();
    Ok(RootDocument {
        root_dir,
        root_name,
        include_paths: options.include_paths.clone(),
        stream_mode: options.stream_mode,
    })
}

#[cfg(feature = "freetype")]
pub struct KpseFontResolver {
    kpsewhich: Option<PathBuf>,
}

#[cfg(feature = "freetype")]
impl KpseFontResolver {
    pub fn detect() -> Option<Self> {
        Some(Self {
            kpsewhich: which_kpsewhich(),
        })
    }

    pub fn dummy() -> Self {
        Self { kpsewhich: None }
    }
}

#[cfg(feature = "freetype")]
fn which_kpsewhich() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OXIPRESSO_KPSEWHICH") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let output = std::process::Command::new("kpsewhich")
        .arg("--version")
        .output()
        .ok()?;
    output.status.success().then(|| PathBuf::from("kpsewhich"))
}

#[cfg(feature = "freetype")]
impl GlyphFontResolver for KpseFontResolver {
    fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>> {
        // The engine sometimes records the font name as the RESOLVED PATH
        // (ctex-fontset-windows emits e.g. `C:/WINDOWS/fonts\simhei.ttf`) —
        // such a name is used verbatim, without a kpsewhich round-trip.
        let normalized = name.replace('\\', "/");
        let direct = PathBuf::from(&normalized);
        if direct.is_file() {
            return std::fs::read(&direct).ok();
        }
        let bare = normalized
            .rsplit('/')
            .next()
            .unwrap_or(&normalized)
            .to_string();
        let kpsewhich = self.kpsewhich.as_ref()?;
        for extension in extensions {
            let output = std::process::Command::new(kpsewhich)
                .arg(format!("{name}.{extension}"))
                .output()
                .ok()?;
            if !output.status.success() {
                continue;
            }
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let resolved = stdout.lines().map(str::trim).find(|line| !line.is_empty());
            if let Some(path) = resolved
                && let Ok(bytes) = std::fs::read(path)
            {
                return Some(bytes);
            }
        }
        // Last resort: the Windows system font directory by bare name —
        // SimSun/SimHei and friends live there, outside kpathsea databases.
        for extension in extensions {
            let candidate = PathBuf::from(r"C:\Windows\Fonts").join(format!("{bare}.{extension}"));
            if candidate.is_file() {
                return std::fs::read(&candidate).ok();
            }
        }
        None
    }
}

/// Loads image files referenced by `pdf:image` specials. Paths are resolved
/// against the root document's directory first (the specials usually carry
/// paths relative to the TeX source), then against the include paths.
#[cfg(feature = "freetype")]
pub struct DocumentImageLoader {
    pub roots: Vec<PathBuf>,
}

#[cfg(feature = "freetype")]
impl GlyphImageLoader for DocumentImageLoader {
    fn find_image_file(&mut self, path: &str) -> Option<Vec<u8>> {
        let relative = std::path::Path::new(path);
        for root in &self.roots {
            let candidate = root.join(relative);
            if let Ok(bytes) = std::fs::read(&candidate) {
                return Some(bytes);
            }
            // Also try the bare path (absolute or cwd-relative).
            if root == &self.roots[0]
                && let Ok(bytes) = std::fs::read(relative)
            {
                return Some(bytes);
            }
        }
        None
    }
}

/// Maps engine output-stream events onto the editor info buffers, mirroring
/// the original protocol: stdout becomes `out`, the `.log` file becomes
/// `log`; both buffers are truncated at the start of a run and flushed at
/// the end. Other writes (XDV, synctex, aux) are not editor channels.
///
/// In `line_output` mode the original only forwards *complete* lines (the
/// editor buffers accumulate until a newline), so trailing partial lines are
/// withheld and the appends become `append-lines`.
fn stream_messages_from_events(events: Vec<OutputEvent>, line_output: bool) -> Vec<EditorMessage> {
    if events.is_empty() {
        return Vec::new();
    }

    let mut out_text = String::new();
    let mut log_text = String::new();
    for event in &events {
        let Ok(text) = String::from_utf8(event.data.clone()) else {
            continue;
        };
        if event.path == "stdout" {
            out_text.push_str(&text);
        } else if event.path.replace('\\', "/").ends_with(".log") {
            log_text.push_str(&text);
        }
    }

    let mut messages = Vec::new();
    if line_output {
        messages.push(EditorMessage::TruncateLines {
            buffer: InfoBuffer::Out,
            count: 0,
        });
        messages.push(EditorMessage::TruncateLines {
            buffer: InfoBuffer::Log,
            count: 0,
        });
        for (buffer, text) in [(InfoBuffer::Out, &out_text), (InfoBuffer::Log, &log_text)] {
            if text.is_empty() {
                continue;
            }
            let mut lines: Vec<String> =
                text.split_inclusive('\n').map(ToOwned::to_owned).collect();
            // Withhold a trailing partial line until its newline arrives.
            if let Some(last) = lines.last()
                && !last.ends_with('\n')
            {
                lines.pop();
            }
            if lines.is_empty() {
                continue;
            }
            messages.push(EditorMessage::AppendLines {
                buffer,
                lines: lines
                    .into_iter()
                    .map(|line| line.trim_end_matches('\n').to_string())
                    .collect(),
            });
        }
    } else {
        messages.push(EditorMessage::Truncate {
            buffer: InfoBuffer::Out,
            size: 0,
        });
        messages.push(EditorMessage::Truncate {
            buffer: InfoBuffer::Log,
            size: 0,
        });
        for event in events {
            let buffer = if event.path == "stdout" {
                InfoBuffer::Out
            } else if event.path.replace('\\', "/").ends_with(".log") {
                InfoBuffer::Log
            } else {
                continue;
            };
            let pos = event.offset;
            if let Ok(text) = String::from_utf8(event.data) {
                messages.push(EditorMessage::Append { buffer, pos, text });
            }
        }
    }
    messages.push(EditorMessage::Flush);
    messages
}

pub struct OxipressoApp {
    options: CliOptions,
    root: RootDocument,
    vfs: VirtualFileSystem,
    engine: Box<dyn TypesettingEngine>,
    viewer: ViewerState,
    renderer: AutoRenderBackend,
    synctex: Option<SyncTexDocument>,
    paused: bool,
    /// Live resident session (P0.3): the engine runs on the session worker
    /// thread; editor changes become hot passes instead of full restarts.
    /// None in stub/external mode or when OXIPRESSO_RESIDENT is unset.
    resident: Option<oxipresso_engine_xetex::ResidentSession>,
    /// Canonical paths the session engine read during its latest pass — the
    /// read_files rebuild-skip set for the resident path.
    resident_read_files: Option<std::collections::HashSet<String>>,
    /// The freshest artifact a resident pass produced (postamble-pending
    /// until the session finishes). `current_artifact` serves it while a
    /// session is live.
    resident_artifact: Option<oxipresso_engine_api::DocumentArtifact>,
}

/// A VFS configured the way the engine should see the document world: disk
/// roots from the document + include paths, and the selected package
/// provider's resolver (TeX Live via kpsewhich, or a local Tectonic bundle
/// directory). Used for the app's protocol-side VFS and for the resident
/// session's worker (engine-I/O) VFS.
fn engine_vfs(root: &RootDocument, provider: PackageProvider) -> Result<VirtualFileSystem, String> {
    let mut vfs = VirtualFileSystem::new();
    vfs.set_disk_roots(disk_roots_for(root));
    match provider {
        PackageProvider::Tectonic => {
            oxipresso_engine_xetex::tectonic::DirBundleResolver::check_env()?;
            vfs.set_resolver(Box::new(
                oxipresso_engine_xetex::tectonic::DirBundleResolver::from_env()
                    .expect("bundle env checked above"),
            ));
        }
        PackageProvider::Texlive | PackageProvider::Auto => {
            if let Some(resolver) = oxipresso_engine_xetex::texlive::KpsewhichResolver::auto() {
                vfs.set_resolver(Box::new(resolver));
            }
        }
    }
    Ok(vfs)
}

impl OxipressoApp {
    pub fn new(options: CliOptions, root: RootDocument) -> Self {
        let paused = options.stream_mode;
        // run_with_io validated the provider env before constructing the app;
        // direct construction (tests, embedders) degrades to Auto resolution
        // with a loud warning instead of failing the constructor.
        let vfs = match engine_vfs(&root, options.provider) {
            Ok(vfs) => vfs,
            Err(error) => {
                eprintln!("oxipresso: {error}; falling back to default resolution");
                match engine_vfs(&root, PackageProvider::Auto) {
                    Ok(vfs) => vfs,
                    Err(error) => {
                        eprintln!("oxipresso: {error}; continuing without a resolver");
                        VirtualFileSystem::new()
                    }
                }
            }
        };
        let renderer = {
            let renderer = AutoRenderBackend::default();
            #[cfg(feature = "freetype")]
            let renderer =
                renderer.with_xdv_glyph_backend(XdvGlyphRenderBackend::with_image_loader(
                    Box::new(
                        KpseFontResolver::detect().unwrap_or_else(|| KpseFontResolver::dummy()),
                    ),
                    Box::new(DocumentImageLoader {
                        roots: disk_roots_for(&root),
                    }),
                ));
            renderer
        };
        Self {
            options,
            root,
            vfs,
            engine: choose_engine(),
            viewer: ViewerState::default(),
            renderer,
            synctex: None,
            paused,
            resident: None,
            resident_artifact: None,
            resident_read_files: None,
        }
    }

    pub fn viewer_state(&self) -> &ViewerState {
        &self.viewer
    }

    /// The freshest document artifact: the resident session's last pass
    /// while a session is live (its XDV is postamble-pending until the
    /// session finishes), otherwise the engine's own output.
    pub fn current_artifact(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
        if self.resident.is_some() {
            return self.resident_artifact.clone();
        }
        self.engine.output_document()
    }

    pub fn initialize(&mut self) -> Result<Vec<EditorMessage>, String> {
        let mut messages = Vec::new();
        if !self.paused {
            self.prime_root_from_disk()?;
            if self.try_start_resident_session()? {
                // Release the capture park IMMEDIATELY by submitting the
                // primed root: pass 1 is the first hot pass. (Without this
                // the worker idles at the capture park for its full 60s
                // fence deadline before falling through — the GUI startup
                // appeared hung for a whole minute.)
                let root_path = self.root.root_name.clone();
                messages.extend(self.resident_rebuild(&root_path)?);
            } else {
                messages.extend(self.initialize_engine()?);
            }
        }
        messages.extend(self.drain_input_messages());
        messages.extend(self.drain_lookup_messages());
        Ok(messages)
    }

    /// Whether resident hot-reload mode is requested (`OXIPRESSO_RESIDENT=1`).
    fn resident_requested(&self) -> bool {
        matches!(std::env::var("OXIPRESSO_RESIDENT").as_deref(), Ok("1"))
    }

    /// Start the resident session if requested and possible. Returns false
    /// (with the normal full-restart path left intact) when the mode is not
    /// requested, the stub/external engine is in use, or the prebuilt format
    /// file is missing — a session cannot bootstrap a format (the INI run
    /// would park forever waiting for a submit that never comes).
    fn try_start_resident_session(&mut self) -> Result<bool, String> {
        if self.resident.is_some() || !self.resident_requested() {
            return Ok(self.resident.is_some());
        }
        if !oxipresso_engine_xetex::XetexEngine::real_mode() {
            return Ok(false);
        }
        // The worker VFS is the engine's I/O surface; prime it with the same
        // current root bytes the protocol-side VFS holds. In stream mode the
        // editor has not delivered content yet, so the session starts on the
        // normal path instead (slice-1 scope: non-stream live rebuilds).
        let root_path = self.root.root_name.clone();
        let Some(bytes) = self.root_bytes_from_vfs(&root_path)? else {
            return Ok(false);
        };
        let mut worker_vfs = engine_vfs(&self.root, self.options.provider)?;
        worker_vfs.open_editor(&self.root.root_name, bytes);
        let engine = oxipresso_engine_xetex::XetexEngine::new();
        let session = oxipresso_engine_xetex::XetexEngine::start_resident_session(
            engine,
            worker_vfs,
            self.root.clone(),
        )
        .map_err(|e| {
            eprintln!("[resident] session unavailable, falling back to full restarts: {e}");
            e.to_string()
        });
        match session {
            Ok(session) => {
                self.resident = Some(session);
                Ok(true)
            }
            Err(error) => {
                let _ = error;
                Ok(false)
            }
        }
    }

    /// Current editor bytes for `path` from the protocol-side VFS.
    fn root_bytes_from_vfs(&mut self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let wanted = canonical_change_key(path);
        let inputs = self.vfs.snapshot_inputs().map_err(|e| e.to_string())?;
        Ok(inputs
            .into_iter()
            .find(|(p, _)| canonical_change_key(p) == wanted)
            .map(|(_, bytes)| bytes))
    }

    /// Wait for the next completed pass's snapshot (the session parks after
    /// every pass; a 120s window covers slow first passes).
    fn wait_resident_snapshot(
        &mut self,
    ) -> Result<oxipresso_engine_xetex::ResidentSnapshot, String> {
        eprintln!("[gui-flow] waiting for the pass snapshot...");
        let session = self
            .resident
            .as_ref()
            .expect("wait_resident_snapshot requires a live session");
        let snap = session
            .snapshots()
            .recv_timeout(std::time::Duration::from_secs(120))
            .map_err(|e| e.to_string());
        eprintln!("[gui-flow] snapshot received");
        snap
    }

    /// Convert one completed pass's snapshot into editor messages and refresh
    /// the viewer/synctex/persisted-output state from it — the resident
    /// counterpart of `messages_after_engine_run`.
    fn messages_from_snapshot(
        &mut self,
        snap: oxipresso_engine_xetex::ResidentSnapshot,
    ) -> Result<Vec<EditorMessage>, String> {
        let artifact = snap.document_artifact(&self.root.root_name);
        let synctex = snap.synctex_artifact();
        if let Some(artifact) = &artifact {
            self.persist_bytes_if_requested("OXIPRESSO_ARTIFACT_OUT", &artifact.bytes)?;
        }
        if let Some(synctex) = &synctex {
            self.persist_bytes_if_requested("OXIPRESSO_SYNCTEX_OUT", &synctex.bytes)?;
        }
        // Serve the freshest pass output to consumers (the GUI pulls it
        // through `current_artifact` after each handled wire line).
        self.resident_artifact = artifact.clone();
        // The pass's read set powers the resident rebuild-skip: an edit to a
        // file outside it cannot change the output.
        self.resident_read_files = Some(snap.read_files.iter().cloned().collect());
        // The stream messages carry the pass's out/log appends; the file
        // notifications (lookup/input) follow, matching the full-restart
        // flow's drain order.
        let mut messages =
            stream_messages_from_events(snap.output_events, self.options.line_output);
        for event in snap.io_events {
            messages.push(match event {
                oxipresso_engine_api::EngineIoEvent::Lookup { kind, status, path } => {
                    EditorMessage::LookupFile {
                        kind: match kind {
                            oxipresso_engine_api::IoLookupKind::Read => {
                                oxipresso_editor_protocol::LookupKind::Read
                            }
                            oxipresso_engine_api::IoLookupKind::Write => {
                                oxipresso_editor_protocol::LookupKind::Write
                            }
                        },
                        status: match status {
                            oxipresso_engine_api::IoLookupStatus::Successful => {
                                oxipresso_editor_protocol::LookupStatus::Successful
                            }
                            oxipresso_engine_api::IoLookupStatus::Failed => {
                                oxipresso_editor_protocol::LookupStatus::Failed
                            }
                            oxipresso_engine_api::IoLookupStatus::Promised => {
                                oxipresso_editor_protocol::LookupStatus::Promised
                            }
                        },
                        path,
                    }
                }
                oxipresso_engine_api::EngineIoEvent::Input { index, path } => {
                    EditorMessage::InputFile { index, path }
                }
            });
        }
        if let Some(artifact) = artifact {
            self.viewer
                .load_artifact_with_renderer(artifact, &self.renderer)
                .map_err(|e| e.to_string())?;
        }
        if let Some(synctex) = synctex {
            self.viewer.sync_position = None;
            self.synctex = oxipresso_synctex::parse_artifact(&synctex).ok();
        }
        Ok(messages)
    }

    /// Write `bytes` to the env-named output path when set (the shared write
    /// path of the artifact/synctex persistence helpers).
    fn persist_bytes_if_requested(&self, env_key: &str, bytes: &[u8]) -> Result<(), String> {
        let Some(path) = std::env::var_os(env_key) else {
            return Ok(());
        };
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, bytes).map_err(|e| e.to_string())
    }

    /// Hot rebuild: submit the changed file's current bytes to the resident
    /// session and wait for the pass. The fence injects the bytes into the
    /// worker VFS, restores the S0 checkpoint, and re-typesets — no format
    /// reload.
    fn resident_rebuild(&mut self, path: &str) -> Result<Vec<EditorMessage>, String> {
        let bytes = self
            .root_bytes_from_vfs(path)?
            .ok_or_else(|| format!("changed file {path} is not in the editor set"))?;
        eprintln!("[hotpass] submit {} bytes={}", path, bytes.len());
        {
            let session = self
                .resident
                .as_ref()
                .expect("resident_rebuild needs a live session");
            let parks = session.parks();
            session.submit(path, bytes);
            session.wait_parks(parks + 1);
        }
        let snap = self.wait_resident_snapshot()?;
        self.messages_from_snapshot(snap)
    }

    pub fn handle_editor_line(&mut self, line: &str) -> Result<Vec<EditorMessage>, String> {
        // Match TeXpresso (src/frontend/editor.c): a malformed or unknown
        // command is reported to stderr and skipped -- the editor session keeps
        // running rather than aborting on a single bad line. Only genuine
        // engine/VFS faults from `handle_command` remain fatal to the run.
        let command = match parse_command(line, self.options.protocol) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("[command] {error}");
                return Ok(Vec::new());
            }
        };
        self.handle_command(command)
    }

    fn handle_command(&mut self, command: EditorCommand) -> Result<Vec<EditorMessage>, String> {
        let mut messages = Vec::new();
        match &command {
            EditorCommand::Pause => {
                self.paused = true;
                return Ok(messages);
            }
            EditorCommand::Resume => {
                self.paused = false;
                self.prime_root_from_disk()?;
                if self.resident.is_some() || self.try_start_resident_session()? {
                    // Hot path: the session (new or existing) typesets the
                    // current content as one pass — a paused span worth of
                    // edits folds into a single rebuild.
                    let root_path = self.root.root_name.clone();
                    messages.extend(self.resident_rebuild(&root_path)?);
                } else {
                    messages.extend(self.initialize_engine()?);
                }
                messages.extend(self.drain_input_messages());
                messages.extend(self.drain_lookup_messages());
                return Ok(messages);
            }
            EditorCommand::PreviousPage => self.viewer.previous_page(),
            EditorCommand::NextPage => self.viewer.next_page(),
            EditorCommand::Crop => self.viewer.toggle_crop(),
            EditorCommand::Invert => self.viewer.toggle_invert(),
            EditorCommand::Theme { bg, fg } => {
                // TeXpresso (main.c) converts the editor's bg/fg floats to an
                // 0xRRGGBB canvas background + default ink and repaints. Apply
                // it to the renderer; convert_color is clamp(0..1)*255.
                self.viewer.themed = true;
                let to_rgb = |c: &[f32; 3]| {
                    [
                        (c[0].clamp(0.0, 1.0) * 255.0) as u8,
                        (c[1].clamp(0.0, 1.0) * 255.0) as u8,
                        (c[2].clamp(0.0, 1.0) * 255.0) as u8,
                    ]
                };
                self.renderer.set_theme(to_rgb(bg), to_rgb(fg));
            }
            EditorCommand::SynctexForward { path, line } => self.apply_synctex_forward(path, *line),
            _ => {}
        }

        // TeXpresso's change appliers report invalid offsets/lines/not-open or
        // bad base64 with "…skipping" to stderr and keep serving; a semantically
        // invalid (but well-formed) editor command must not kill the long-lived
        // session. Only genuine engine faults below remain fatal.
        let outcome = match self.vfs.apply_editor_command(&command) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("[command] {error}");
                None
            }
        };
        // TeXpresso rescan schedules a disk scan so external on-disk edits (a
        // file changed outside the editor open/change stream) are picked up
        // before rebuilding. Reload the root from disk best-effort so the
        // rebuild sees current bytes, not the stale VFS copy.
        if matches!(command, EditorCommand::Rescan) {
            let _ = self.prime_root_from_disk();
        }
        // Resident path (P0.3): any engine-relevant change becomes one hot
        // pass — the fence injects the edited bytes into the worker VFS and
        // restores the checkpoint; no format reload. The hot pass targets
        // the CHANGED file (an include edit must reach the engine), and the
        // pass's read_files set skips rebuilds for files the engine never
        // read (the session's own read tracking; the main-side engine never
        // ran).
        if self.resident.is_some() {
            let changed_path = match &command {
                EditorCommand::Rescan => Some(self.root.root_name.clone()),
                _ => outcome
                    .as_ref()
                    .and_then(|o| o.changed_offset.map(|_| o.path.clone())),
            };
            if let Some(target) = changed_path
                && !self.paused
            {
                let relevant = match &self.resident_read_files {
                    Some(read) => read.contains(&canonical_change_key(&target)),
                    None => true, // no pass snapshot yet — rebuild to learn it
                };
                if relevant {
                    messages.extend(self.resident_rebuild(&target)?);
                }
            }
            messages.extend(self.drain_input_messages());
            messages.extend(self.drain_lookup_messages());
            return Ok(messages);
        }
        let mut restart_policy =
            matches!(command, EditorCommand::Rescan).then_some(RestartPolicy::FullRestartRequired);
        if let Some(ChangeOutcome {
            path,
            changed_offset: Some(offset),
        }) = outcome
        {
            restart_policy = Some(
                self.engine
                    .apply_change_hint(&oxipresso_engine_api::PathId(path), offset)
                    .map_err(|e| e.to_string())?,
            );
        }
        if let Some(policy) = restart_policy
            && !self.paused
        {
            messages.extend(self.rebuild(policy)?);
        }
        messages.extend(self.drain_input_messages());
        messages.extend(self.drain_lookup_messages());
        Ok(messages)
    }

    fn drain_lookup_messages(&mut self) -> Vec<EditorMessage> {
        self.vfs
            .take_lookup_events()
            .into_iter()
            .map(|event| EditorMessage::LookupFile {
                kind: event.kind,
                status: event.status,
                path: event.path,
            })
            .collect()
    }

    fn drain_input_messages(&mut self) -> Vec<EditorMessage> {
        self.vfs
            .take_input_events()
            .into_iter()
            .map(|event| EditorMessage::InputFile {
                index: event.index,
                path: event.path,
            })
            .collect()
    }

    fn prime_root_from_disk(&mut self) -> Result<(), String> {
        if self.options.stream_mode {
            return Ok(());
        }
        let bytes = std::fs::read(self.root.root_dir.join(&self.root.root_name))
            .map_err(|e| e.to_string())?;
        self.vfs.open_editor(&self.root.root_name, bytes);
        Ok(())
    }

    fn persist_artifact_if_requested(&self) -> Result<(), String> {
        let Some(path) = std::env::var_os("OXIPRESSO_ARTIFACT_OUT") else {
            return Ok(());
        };
        let Some(artifact) = self.engine.output_document() else {
            return Ok(());
        };
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, artifact.bytes).map_err(|e| e.to_string())
    }

    fn persist_synctex_if_requested(&self) -> Result<(), String> {
        let Some(path) = std::env::var_os("OXIPRESSO_SYNCTEX_OUT") else {
            return Ok(());
        };
        let Some(synctex) = self.engine.output_synctex() else {
            return Ok(());
        };
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, synctex.bytes).map_err(|e| e.to_string())
    }

    fn refresh_viewer_from_artifact(&mut self) -> Result<(), String> {
        let Some(artifact) = self.engine.output_document() else {
            return Ok(());
        };
        self.viewer
            .load_artifact_with_renderer(artifact, &self.renderer)
            .map_err(|e| e.to_string())
    }

    fn refresh_synctex_from_engine(&mut self) {
        self.viewer.sync_position = None;
        self.synctex = self
            .engine
            .output_synctex()
            .and_then(|artifact| oxipresso_synctex::parse_artifact(&artifact).ok());
    }

    fn apply_synctex_forward(&mut self, path: &str, line: usize) {
        let Some(hit) = self
            .synctex
            .as_ref()
            .and_then(|synctex| synctex.forward_search_path(path, line))
        else {
            return;
        };
        self.viewer.set_sync_position(ViewerSyncPosition {
            page: hit.page.saturating_sub(1),
            path: hit.path,
            line: hit.line,
            x: hit.x,
            y: hit.y,
            width: hit.width,
            height: hit.height,
            depth: hit.depth,
        });
    }

    /// Resolves a click on `page` (1-based SyncTeX numbering) at page
    /// coordinates in points to a source location through reverse SyncTeX,
    /// returning the editor notification message for it. This is the
    /// engine-to-editor half of bidirectional sync: the viewer reports where
    /// the user clicked, the editor jumps to the source line.
    pub fn synctex_reverse_message(
        &self,
        page: usize,
        x_pt: f64,
        y_pt: f64,
    ) -> Option<EditorMessage> {
        let synctex = self.synctex.as_ref()?;
        // SyncTeX coordinates are 1/65536 pt, origin at the page top-left,
        // y growing downward — the same convention as the DVI renderer.
        let x_sp = (x_pt * 65536.0) as i32;
        let y_sp = (y_pt * 65536.0) as i32;
        let hit = synctex.reverse_search_page_point(page, x_sp, y_sp)?;
        Some(EditorMessage::Synctex {
            path: hit.path,
            line: hit.line,
            column: 0,
        })
    }

    fn rebuild(&mut self, policy: RestartPolicy) -> Result<Vec<EditorMessage>, String> {
        let engine_result = match policy {
            RestartPolicy::NoRestartNeeded => return Ok(Vec::new()),
            RestartPolicy::RestartRequired => self.engine.restart(&mut self.vfs),
            RestartPolicy::FullRestartRequired => return self.initialize_engine(),
        };
        self.messages_after_engine_run(engine_result)
    }

    fn initialize_engine(&mut self) -> Result<Vec<EditorMessage>, String> {
        let engine_result = self
            .engine
            .initialize(&self.root, &mut self.vfs)
            .map(|_| ());
        self.messages_after_engine_run(engine_result)
    }

    fn messages_after_engine_run(
        &mut self,
        engine_result: oxipresso_engine_api::Result<()>,
    ) -> Result<Vec<EditorMessage>, String> {
        let stream_messages = self.stream_output_messages();
        let has_stream_messages = !stream_messages.is_empty();
        let mut messages = stream_messages;
        if !has_stream_messages {
            // Engines that do not stream their output channels (or runs that
            // produced no stream writes) fall back to the diagnostics summary.
            messages.extend(self.engine_output_messages());
        }
        if let Err(error) = engine_result {
            if messages.is_empty() {
                messages.extend(self.error_output_messages(error.to_string()));
            }
            return Ok(messages);
        }

        self.persist_artifact_if_requested()?;
        self.persist_synctex_if_requested()?;
        self.refresh_synctex_from_engine();
        self.refresh_viewer_from_artifact()?;
        Ok(messages)
    }

    /// Converts the engine's incremental output-stream writes into editor
    /// messages, mirroring the original protocol: stdout maps to the `out`
    /// buffer, the `.log` file to `log`, a truncate of both buffers starts a
    /// fresh run, and a flush ends it.
    fn stream_output_messages(&mut self) -> Vec<EditorMessage> {
        stream_messages_from_events(self.engine.take_output_events(), self.options.line_output)
    }

    fn engine_output_messages(&self) -> Vec<EditorMessage> {
        let diagnostics = self.engine.diagnostics();
        if diagnostics.is_empty() {
            return Vec::new();
        }

        let text = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join("");

        if self.options.line_output {
            let lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
            vec![
                EditorMessage::TruncateLines {
                    buffer: InfoBuffer::Out,
                    count: 0,
                },
                EditorMessage::AppendLines {
                    buffer: InfoBuffer::Out,
                    lines,
                },
                EditorMessage::Flush,
            ]
        } else {
            vec![
                EditorMessage::Truncate {
                    buffer: InfoBuffer::Out,
                    size: 0,
                },
                EditorMessage::Append {
                    buffer: InfoBuffer::Out,
                    pos: 0,
                    text,
                },
                EditorMessage::Flush,
            ]
        }
    }

    fn error_output_messages(&self, error: String) -> Vec<EditorMessage> {
        if self.options.line_output {
            vec![
                EditorMessage::TruncateLines {
                    buffer: InfoBuffer::Out,
                    count: 0,
                },
                EditorMessage::AppendLines {
                    buffer: InfoBuffer::Out,
                    lines: vec![error],
                },
                EditorMessage::Flush,
            ]
        } else {
            vec![
                EditorMessage::Truncate {
                    buffer: InfoBuffer::Out,
                    size: 0,
                },
                EditorMessage::Append {
                    buffer: InfoBuffer::Out,
                    pos: 0,
                    text: error,
                },
                EditorMessage::Flush,
            ]
        }
    }
}

fn disk_roots_for(root: &RootDocument) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(root.include_paths.len() + 1);
    roots.push(root.root_dir.clone());
    roots.extend(root.include_paths.iter().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            root.root_dir.join(path)
        }
    }));
    roots
}

fn choose_engine() -> Box<dyn TypesettingEngine> {
    match std::env::var("OXIPRESSO_ENGINE") {
        Ok(value) if value.eq_ignore_ascii_case("external") => Box::new(ExternalEngine::xelatex()),
        _ => Box::new(XetexEngine::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxipresso_engine_api::{
        ArtifactKind, Diagnostic, DiagnosticSeverity, EngineError, EngineEvent, EngineInit,
        EngineIo, FileKind, OpenResult, OutputEvent, PathId, RestartPolicy, SyncTexArtifact,
    };
    use std::{
        cell::Cell,
        fs,
        rc::Rc,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_cli_options() {
        let opts = parse_args([
            "-I",
            "build",
            "-json",
            "-lines",
            "-stream",
            "-gui",
            "-test-initialize",
            "main.tex",
        ])
        .unwrap();
        assert_eq!(opts.protocol, WireProtocol::Json);
        assert!(opts.line_output);
        assert!(opts.stream_mode);
        assert!(opts.initialize_only);
        assert!(opts.gui);
        assert_eq!(opts.include_paths, vec![PathBuf::from("build")]);
    }

    #[test]
    fn rejects_provider_conflict() {
        assert!(parse_args(["-texlive", "-tectonic", "main.tex"]).is_err());
    }

    #[test]
    fn tectonic_provider_is_accepted_and_mutually_exclusive() {
        // `-tectonic` now selects the local-bundle provider (P3): accepted at
        // the flag level; the bundle environment is validated at startup.
        let opts = parse_args(["-tectonic", "main.tex"]).unwrap();
        assert!(matches!(opts.provider, PackageProvider::Tectonic));
        // `-texlive` remains accepted and selects the TeX Live provider.
        let opts = parse_args(["-texlive", "main.tex"]).unwrap();
        assert!(matches!(opts.provider, PackageProvider::Texlive));
        // The two providers are still mutually exclusive.
        assert!(parse_args(["-texlive", "-tectonic", "main.tex"]).is_err());
        assert!(parse_args(["-tectonic", "-texlive", "main.tex"]).is_err());
    }

    #[test]
    fn tectonic_provider_fails_loudly_without_bundle_env() {
        // A misconfigured `-tectonic` run fails at startup, naming the env —
        // never silently falling back to TeX Live resolution.
        let _env = env_lock();
        let saved = std::env::var_os(oxipresso_engine_xetex::tectonic::BUNDLE_ENV);
        unsafe {
            std::env::remove_var(oxipresso_engine_xetex::tectonic::BUNDLE_ENV);
        }
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\relax\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Tectonic,
            initialize_only: true,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let mut output = Vec::new();
        let result = run_with_io(options, std::io::Cursor::new(Vec::<u8>::new()), &mut output);
        if let Some(value) = saved {
            unsafe {
                std::env::set_var(oxipresso_engine_xetex::tectonic::BUNDLE_ENV, value);
            }
        }
        let error = result.expect_err("missing bundle env must fail the run");
        assert!(
            error.contains(oxipresso_engine_xetex::tectonic::BUNDLE_ENV),
            "error names the bundle env, got: {error}"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn rejects_malformed_arguments() {
        // Each of these must be a clean Err, not a panic or a silent misparse —
        // they guard the CLI input contract against user typos.
        assert!(parse_args(["-I"]).is_err(), "-I with no value");
        assert!(
            parse_args(["-I", "build", "-json"]).is_err(),
            "-I consumes 'build', leaving no root document"
        );
        assert!(parse_args(["-nope", "main.tex"]).is_err(), "unknown option");
        assert!(
            parse_args(["a.tex", "b.tex"]).is_err(),
            "two root documents rejected"
        );
        assert!(
            parse_args::<Vec<String>, String>(vec![]).is_err(),
            "no root document"
        );
        // Default (no flags) is a valid Sexp, non-line, non-stream, non-gui run.
        let opts = parse_args(["main.tex"]).unwrap();
        assert_eq!(opts.protocol, WireProtocol::Sexp);
        assert!(!opts.line_output && !opts.stream_mode && !opts.gui && !opts.initialize_only);
    }

    #[test]
    fn register_does_not_emit_until_engine_lookup() {
        let _env = env_lock();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: true,
            gui: false,
            root_file: PathBuf::from("main.tex"),
        };
        let mut output = Vec::new();
        run_with_io(
            options,
            br#"(register "main.tex")
"# as &[u8],
            &mut output,
        )
        .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "");
    }

    #[test]
    fn resume_emits_promised_lookup_after_engine_requests_registered_root() {
        let _env = env_lock();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: true,
            gui: false,
            root_file: PathBuf::from("main.tex"),
        };
        let mut output = Vec::new();
        run_with_io(
            options,
            br#"(register "main.tex")
(resume)
"# as &[u8],
            &mut output,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(r#"(lookup-file read promised "main.tex")"#));
    }

    #[test]
    fn stream_register_open_resume_reads_editor_root() {
        let _env = env_lock();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: true,
            stream_mode: true,
            gui: false,
            root_file: PathBuf::from("main.tex"),
        };
        let mut output = Vec::new();
        run_with_io(
            options,
            br#"(register "main.tex")
(open "main.tex" "\\documentclass{article}\n\\begin{document}\nHi\n\\end{document}\n")
(resume)
"# as &[u8],
            &mut output,
        )
        .unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(r#"(lookup-file read successful "main.tex")"#));
        assert!(output.contains(r#"(input-file 0 "main.tex")"#));
        assert!(output.contains(r#"(lookup-file write successful "stdout")"#));
    }

    #[test]
    fn stream_register_promised_file_then_open_triggers_rebuild() {
        let _env = env_lock();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: true,
            gui: false,
            root_file: PathBuf::from("main.tex"),
        };
        let root = root_document(&options).unwrap();
        let init_count = Rc::new(Cell::new(0));
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(MissingLookupEngine {
            init_count: Rc::clone(&init_count),
            target: "texpresso_ci_missing_file.tex".to_string(),
        });

        assert!(app.initialize().unwrap().is_empty());
        assert!(
            app.handle_editor_line(r#"(register "texpresso_ci_missing_file.tex")"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            app.handle_editor_line(r#"(open "main.tex" "\\input{texpresso_ci_missing_file}\n")"#)
                .unwrap()
                .is_empty()
        );

        let promised_messages = app.handle_editor_line("(resume)").unwrap();
        assert_eq!(init_count.get(), 1);
        assert!(promised_messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::LookupFile {
                    path,
                    status: oxipresso_editor_protocol::LookupStatus::Promised,
                    ..
                } if path == "texpresso_ci_missing_file.tex"
            )
        }));

        let rebuild_messages = app
            .handle_editor_line(r#"(open "texpresso_ci_missing_file.tex" "Included content.\n")"#)
            .unwrap();

        assert_eq!(init_count.get(), 2);
        assert!(rebuild_messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::LookupFile {
                    path,
                    status: oxipresso_editor_protocol::LookupStatus::Successful,
                    ..
                } if path == "texpresso_ci_missing_file.tex"
            )
        }));
        assert!(rebuild_messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::InputFile { path, .. } if path == "texpresso_ci_missing_file.tex"
            )
        }));
    }

    #[test]
    fn initialize_emits_input_file_for_root_read() {
        let _env = env_lock();
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nHi\n\\end{document}\n",
        )
        .unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: true,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let mut output = Vec::new();
        run_with_io(options, std::io::Cursor::new(Vec::<u8>::new()), &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(r#"(input-file 0 "main.tex")"#));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn synctex_reverse_message_converts_points_to_sp_and_guards_missing_sidecar() {
        // Always-run (no real engine / env gate): builds a minimal plain
        // sidecar, verifies `synctex_reverse_message` scales viewer points to
        // SyncTeX sp units (x_sp = pt * 65536), picks the nearest record, and
        // returns None when no sidecar is loaded (early-session click).
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\documentclass{article}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);

        // No sidecar yet: a click resolves to None, never panics.
        assert!(
            app.synctex_reverse_message(1, 300.0, 300.0).is_none(),
            "reverse search with no sidecar must be None"
        );

        // Record A at (300pt, 300pt) => sp 300*65536; record B at (1000pt).
        let sidecar = "SyncTeX Version:1\n\
Output:main.pdf\n\
Input:1:main.tex\n\
Content:\n\
{1\n\
g1,10:19660800,19660800\n\
g1,20:65536000,65536000\n\
}\n";
        let artifact = oxipresso_engine_api::SyncTexArtifact {
            bytes: sidecar.as_bytes().to_vec(),
            compressed: false,
            source_name: None,
        };
        app.synctex = oxipresso_synctex::parse_artifact(&artifact).ok();
        assert!(app.synctex.is_some(), "minimal sidecar should parse");

        // pt 300 => exactly record A.
        match app
            .synctex_reverse_message(1, 300.0, 300.0)
            .expect("hit at record A's point")
        {
            EditorMessage::Synctex { path, line, .. } => {
                assert_eq!(path, "main.tex");
                assert_eq!(line, 10, "300pt maps to record A line 10");
            }
            other => panic!("expected synctex message, got {other:?}"),
        }
        // pt 990 is nearer record B (1000pt) than A (300pt) => line 20.
        match app
            .synctex_reverse_message(1, 990.0, 990.0)
            .expect("hit at nearest record B")
        {
            EditorMessage::Synctex { line, .. } => {
                assert_eq!(line, 20, "nearest-record selection must scale correctly");
            }
            other => panic!("expected synctex message, got {other:?}"),
        }
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn json_protocol_initialization_serializes_messages_as_json() {
        let _env = env_lock();
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nHi\n\\end{document}\n",
        )
        .unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Json,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: true,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let mut output = Vec::new();

        run_with_io(options, std::io::Cursor::new(Vec::<u8>::new()), &mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(r#"["input-file",0,"main.tex"]"#));
        assert!(output.contains(r#"["lookup-file","read","successful","main.tex"]"#));
        assert!(output.contains(r#"["lookup-file","write","successful","stdout"]"#));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn initialize_refreshes_viewer_state_from_engine_artifact() {
        let temp_dir = unique_temp_dir();
        std::fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        std::fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nHi\n\\end{document}\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let artifact = two_page_pdf_artifact();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(ArtifactEngine { artifact });

        app.initialize().unwrap();

        assert_eq!(app.viewer_state().page_count, 2);
        std::fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn initialize_engine_error_emits_diagnostics_without_failing_session() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nBroken\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(FailingDiagnosticEngine {
            diagnostics: vec![Diagnostic {
                severity: DiagnosticSeverity::Error,
                message: "TeX said nope".to_string(),
                path: Some("main.tex".to_string()),
                line: Some(3),
            }],
        });

        let messages = app.initialize().unwrap();

        assert!(messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::Append { text, .. } if text.contains("TeX said nope")
            )
        }));
        assert!(messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::LookupFile {
                    path,
                    status: oxipresso_editor_protocol::LookupStatus::Successful,
                    ..
                } if path == "main.tex"
            )
        }));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn change_triggering_a_failed_rebuild_does_not_abort_session() {
        // The most important live-preview invariant: the user edits the document
        // into a broken state (engine compile fails) and the session must keep
        // serving with the error reported, not crash. Covers the change->rebuild
        // path (distinct from the initialize-engine error test above).
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\documentclass{article}\n\\begin{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(FailingDiagnosticEngine {
            diagnostics: vec![Diagnostic {
                severity: DiagnosticSeverity::Error,
                message: "Undefined control sequence".to_string(),
                path: Some("main.tex".to_string()),
                line: Some(3),
            }],
        });
        // Initialize (engine fails) is already survivable; now edit the open file.
        let _ = app.initialize().unwrap();
        let messages = app
            .handle_editor_line(r#"(change "main.tex" 0 0 "\undefinedmacro")"#)
            .expect("a change that fails the rebuild must not abort the session");
        assert!(
            messages.iter().any(|message| {
                matches!(
                    message,
                    EditorMessage::Append { text, .. }
                        if text.contains("Undefined control sequence")
                )
            }),
            "failed rebuild reports diagnostics: {messages:?}"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn stream_events_split_into_out_and_log_buffers() {
        let messages = stream_messages_from_events(
            vec![
                OutputEvent {
                    path: "stdout".to_string(),
                    offset: 0,
                    data: b"This is XeTeX\n".to_vec(),
                },
                OutputEvent {
                    path: "main.log".to_string(),
                    offset: 0,
                    data: b"log line\n".to_vec(),
                },
                OutputEvent {
                    path: "main.xdv".to_string(),
                    offset: 0,
                    data: vec![0xF7, 0x07],
                },
                OutputEvent {
                    path: "stdout".to_string(),
                    offset: 14,
                    data: "Output written\n".as_bytes().to_vec(),
                },
            ],
            false,
        );
        assert_eq!(
            messages.first(),
            Some(&EditorMessage::Truncate {
                buffer: InfoBuffer::Out,
                size: 0
            })
        );
        assert_eq!(
            messages.get(1),
            Some(&EditorMessage::Truncate {
                buffer: InfoBuffer::Log,
                size: 0
            })
        );
        let appends: Vec<&EditorMessage> = messages
            .iter()
            .filter(|message| matches!(message, EditorMessage::Append { .. }))
            .collect();
        assert_eq!(appends.len(), 3);
        assert!(matches!(
            appends[0],
            EditorMessage::Append {
                buffer: InfoBuffer::Out,
                ..
            }
        ));
        assert!(matches!(
            appends[1],
            EditorMessage::Append {
                buffer: InfoBuffer::Log,
                ..
            }
        ));
        assert!(matches!(
            appends[2],
            EditorMessage::Append {
                buffer: InfoBuffer::Out,
                ..
            }
        ));
        assert!(matches!(messages.last(), Some(EditorMessage::Flush)));
    }

    #[test]
    fn stream_events_empty_produces_no_messages() {
        assert!(stream_messages_from_events(Vec::new(), false).is_empty());
        assert!(stream_messages_from_events(Vec::new(), true).is_empty());
    }

    #[test]
    fn synctex_reverse_search_maps_page_point_to_source() {
        // OXIPRESSO_SYNCTEX_SMOKE points at a real .synctex sidecar
        // (e.g. produced by the real engine through OXIPRESSO_SYNCTEX_OUT).
        let Ok(path) = std::env::var("OXIPRESSO_SYNCTEX_SMOKE") else {
            return;
        };
        let Ok(bytes) = fs::read(&path) else {
            return;
        };

        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\documentclass{article}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        let artifact = oxipresso_engine_api::SyncTexArtifact {
            bytes,
            compressed: false,
            source_name: None,
        };
        app.synctex = oxipresso_synctex::parse_artifact(&artifact).ok();

        // A click in the middle of page 1 must resolve to a source location.
        let message = app
            .synctex_reverse_message(1, 300.0, 300.0)
            .expect("reverse search should find the nearest record");
        match message {
            EditorMessage::Synctex { path, line, .. } => {
                assert!(path.ends_with(".tex"), "reverse hit path {path:?}");
                assert!(line >= 1, "reverse hit line {line}");
            }
            other => panic!("expected a synctex message, got {other:?}"),
        }
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn stream_events_line_mode_forwards_complete_lines_only() {
        let events = vec![
            OutputEvent {
                path: "stdout".to_string(),
                offset: 0,
                data: b"first line\nsecond".to_vec(),
            },
            OutputEvent {
                path: "main.log".to_string(),
                offset: 0,
                data: b"log line\n".to_vec(),
            },
        ];
        let messages = stream_messages_from_events(events, true);
        assert!(matches!(
            messages.first(),
            Some(EditorMessage::TruncateLines {
                buffer: InfoBuffer::Out,
                count: 0
            })
        ));
        let out_lines = messages.iter().find_map(|message| match message {
            EditorMessage::AppendLines {
                buffer: InfoBuffer::Out,
                lines,
            } => Some(lines.clone()),
            _ => None,
        });
        assert_eq!(
            out_lines,
            Some(vec!["first line".to_string()]),
            "partial trailing line must be withheld"
        );
        let log_lines = messages.iter().find_map(|message| match message {
            EditorMessage::AppendLines {
                buffer: InfoBuffer::Log,
                lines,
            } => Some(lines.clone()),
            _ => None,
        });
        assert_eq!(log_lines, Some(vec!["log line".to_string()]));
        assert!(matches!(messages.last(), Some(EditorMessage::Flush)));
    }

    #[test]
    fn stream_line_mode_joins_line_split_across_chunks() {
        // A line split across two stdout events (engine flushed mid-line) must
        // accumulate into one logical line before splitting; per-chunk splitting
        // would wrongly emit "c" and fragment "abc".
        let events = vec![
            OutputEvent {
                path: "stdout".to_string(),
                offset: 0,
                data: b"ab".to_vec(),
            },
            OutputEvent {
                path: "stdout".to_string(),
                offset: 2,
                data: b"c\nde".to_vec(), // completes "abc", starts partial "de"
            },
        ];
        let messages = stream_messages_from_events(events, true);
        let out_lines = messages.iter().find_map(|message| match message {
            EditorMessage::AppendLines {
                buffer: InfoBuffer::Out,
                lines,
            } => Some(lines.clone()),
            _ => None,
        });
        assert_eq!(
            out_lines,
            Some(vec!["abc".to_string()]),
            "chunked line must rejoin to 'abc' and withhold trailing 'de'"
        );
    }

    #[test]
    fn line_output_diagnostics_use_line_messages() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nBroken\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: true,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(FailingDiagnosticEngine {
            diagnostics: vec![Diagnostic {
                severity: DiagnosticSeverity::Error,
                message: "first line\nsecond line\n".to_string(),
                path: Some("main.tex".to_string()),
                line: Some(3),
            }],
        });

        let messages = app.initialize().unwrap();

        assert!(messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::TruncateLines {
                    buffer: InfoBuffer::Out,
                    count: 0
                }
            )
        }));
        assert!(messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::AppendLines { lines, .. }
                    if lines == &vec!["first line".to_string(), "second line".to_string()]
            )
        }));
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, EditorMessage::Flush))
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn opening_failed_lookup_file_triggers_rebuild() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\input{texpresso_ci_missing_file}\n").unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let init_count = Rc::new(Cell::new(0));
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(MissingLookupEngine {
            init_count: Rc::clone(&init_count),
            target: "texpresso_ci_missing_file.tex".to_string(),
        });

        let initial_messages = app.initialize().unwrap();
        assert_eq!(init_count.get(), 1);
        assert!(initial_messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::LookupFile {
                    path,
                    status: oxipresso_editor_protocol::LookupStatus::Failed,
                    ..
                } if path == "texpresso_ci_missing_file.tex"
            )
        }));

        let rebuild_messages = app
            .handle_editor_line(r#"(open "texpresso_ci_missing_file.tex" "Included content.\n")"#)
            .unwrap();

        assert_eq!(init_count.get(), 2);
        assert!(rebuild_messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::LookupFile {
                    path,
                    status: oxipresso_editor_protocol::LookupStatus::Successful,
                    ..
                } if path == "texpresso_ci_missing_file.tex"
            )
        }));
        assert!(rebuild_messages.iter().any(|message| {
            matches!(
                message,
                EditorMessage::InputFile { path, .. } if path == "texpresso_ci_missing_file.tex"
            )
        }));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn cli_external_engine_compiles_original_include_fixture_with_i_flag_when_available() {
        let _env = env_lock();
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("include.tex") else {
            return;
        };
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let artifact_out = temp_dir.join("out.pdf");
        let previous_engine = std::env::var_os("OXIPRESSO_ENGINE");
        let previous_artifact = std::env::var_os("OXIPRESSO_ARTIFACT_OUT");
        unsafe {
            std::env::set_var("OXIPRESSO_ENGINE", "external");
            std::env::set_var("OXIPRESSO_ARTIFACT_OUT", &artifact_out);
        }

        let result = run_with_io(
            parse_args([
                "-I",
                fixture.parent().unwrap().join("incpath").to_str().unwrap(),
                "-test-initialize",
                fixture.to_str().unwrap(),
            ])
            .unwrap(),
            std::io::Cursor::new(Vec::<u8>::new()),
            Vec::new(),
        );

        restore_env_var("OXIPRESSO_ENGINE", previous_engine);
        restore_env_var("OXIPRESSO_ARTIFACT_OUT", previous_artifact);
        result.unwrap();
        assert!(artifact_out.exists());
        assert!(fs::read(&artifact_out).unwrap().starts_with(b"%PDF-"));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn cli_external_engine_persists_synctex_when_requested() {
        let _env = env_lock();
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nHello\n\\end{document}\n",
        )
        .unwrap();
        let synctex_out = temp_dir.join("main.synctex.gz");
        let previous_engine = std::env::var_os("OXIPRESSO_ENGINE");
        let previous_synctex = std::env::var_os("OXIPRESSO_SYNCTEX_OUT");
        unsafe {
            std::env::set_var("OXIPRESSO_ENGINE", "external");
            std::env::set_var("OXIPRESSO_SYNCTEX_OUT", &synctex_out);
        }

        let result = run_with_io(
            parse_args(["-test-initialize", root_file.to_str().unwrap()]).unwrap(),
            std::io::Cursor::new(Vec::<u8>::new()),
            Vec::new(),
        );

        restore_env_var("OXIPRESSO_ENGINE", previous_engine);
        restore_env_var("OXIPRESSO_SYNCTEX_OUT", previous_synctex);
        result.unwrap();
        let bytes = fs::read(&synctex_out).unwrap();
        assert!(!bytes.is_empty());
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn cli_external_engine_reports_missing_input_without_failing_session_when_available() {
        let _env = env_lock();
        if !ExternalEngine::is_available("xelatex") {
            return;
        }
        let Some(fixture) = oxipresso_testkit::original_texpresso_fixture("missing-input.tex")
        else {
            return;
        };
        let previous_engine = std::env::var_os("OXIPRESSO_ENGINE");
        unsafe {
            std::env::set_var("OXIPRESSO_ENGINE", "external");
        }
        let mut output = Vec::new();

        let result = run_with_io(
            parse_args(["-test-initialize", fixture.to_str().unwrap()]).unwrap(),
            std::io::Cursor::new(Vec::<u8>::new()),
            &mut output,
        );

        restore_env_var("OXIPRESSO_ENGINE", previous_engine);
        result.unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("texpresso_ci_missing_file"));
        assert!(output.contains("(flush)"));
    }

    #[test]
    fn editor_change_triggers_full_restart_rebuild() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nA\n\\end{document}\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let init_count = Rc::new(Cell::new(0));
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(CountingArtifactEngine {
            init_count: Rc::clone(&init_count),
            artifact: two_page_pdf_artifact(),
        });

        app.initialize().unwrap();
        assert_eq!(init_count.get(), 1);

        app.handle_editor_line(r#"(change "main.tex" 0 0 "%")"#)
            .unwrap();

        assert_eq!(init_count.get(), 2);
        assert_eq!(app.viewer_state().page_count, 2);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn synctex_forward_updates_viewer_page_from_latest_synctex() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nOne\n\\newpage\nTwo\n\\end{document}\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(SynctexArtifactEngine {
            artifact: two_page_pdf_artifact(),
            synctex: SyncTexArtifact {
                bytes: b"SyncTeX Version:1\nOutput:main.pdf\nInput:1:main.tex\nContent:\n{2\n[1,5:10,20:30,40,0\n}\n"
                    .to_vec(),
                compressed: false,
                source_name: Some("main.synctex".to_string()),
            },
        });

        app.initialize().unwrap();
        assert_eq!(app.viewer_state().page, 0);

        app.handle_editor_line(r#"(synctex-forward "main.tex" 5)"#)
            .unwrap();

        assert_eq!(app.viewer_state().page, 1);
        let sync_position = app.viewer_state().sync_position.as_ref().unwrap();
        assert_eq!(sync_position.page, 1);
        assert_eq!(sync_position.path, "main.tex");
        assert_eq!(sync_position.line, 5);
        assert_eq!(sync_position.x, 10);
        assert_eq!(sync_position.y, 20);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn synctex_forward_matches_editor_path_variants() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nOne\n\\end{document}\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(SynctexArtifactEngine {
            artifact: two_page_pdf_artifact(),
            synctex: SyncTexArtifact {
                bytes: b"SyncTeX Version:1\nOutput:main.pdf\nInput:1:C:\\Users\\demo\\project\\main.tex\nContent:\n{2\ng1,12:111,222\n}\n"
                    .to_vec(),
                compressed: false,
                source_name: Some("main.synctex".to_string()),
            },
        });

        app.initialize().unwrap();
        app.handle_editor_line(r#"(synctex-forward "./project//main.tex" 12)"#)
            .unwrap();

        let sync_position = app.viewer_state().sync_position.as_ref().unwrap();
        assert_eq!(app.viewer_state().page, 1);
        assert_eq!(sync_position.path, "C:\\Users\\demo\\project\\main.tex");
        assert_eq!(sync_position.line, 12);
        assert_eq!(sync_position.x, 111);
        assert_eq!(sync_position.y, 222);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn app_registers_root_and_include_paths_as_vfs_disk_roots() {
        let temp_dir = unique_temp_dir();
        let include_dir = temp_dir.join("inc");
        fs::create_dir_all(&include_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\input{included}\n").unwrap();
        fs::write(include_dir.join("included.tex"), "included").unwrap();

        let options = CliOptions {
            include_paths: vec![PathBuf::from("inc")],
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let app = OxipressoApp::new(options, root);
        assert!(app.vfs.disk_roots().contains(&temp_dir));
        assert!(app.vfs.disk_roots().contains(&include_dir));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    struct ArtifactEngine {
        artifact: oxipresso_engine_api::DocumentArtifact,
    }

    struct CountingArtifactEngine {
        init_count: Rc<Cell<usize>>,
        artifact: oxipresso_engine_api::DocumentArtifact,
    }

    /// Same as `CountingArtifactEngine` but declares that unrelated edits need
    /// no rebuild, exercising the CLI's `NoRestartNeeded` short-circuit.
    struct NoRestartEngine {
        init_count: Rc<Cell<usize>>,
        artifact: oxipresso_engine_api::DocumentArtifact,
    }

    impl TypesettingEngine for NoRestartEngine {
        fn initialize(
            &mut self,
            _root: &RootDocument,
            _io: &mut dyn EngineIo,
        ) -> oxipresso_engine_api::Result<EngineInit> {
            self.init_count.set(self.init_count.get() + 1);
            Ok(EngineInit {
                engine_name: "no-restart-test".to_string(),
            })
        }
        fn step(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<EngineEvent> {
            Ok(EngineEvent::Idle)
        }
        fn apply_change_hint(
            &mut self,
            _changed_file: &PathId,
            _byte_offset: usize,
        ) -> oxipresso_engine_api::Result<RestartPolicy> {
            Ok(RestartPolicy::NoRestartNeeded)
        }
        fn restart(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<()> {
            Ok(())
        }
        fn output_document(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
            Some(self.artifact.clone())
        }
        fn diagnostics(&self) -> &[Diagnostic] {
            &[]
        }
    }

    #[test]
    fn no_restart_hint_skips_rebuild_on_edit() {
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}x\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let init_count = Rc::new(Cell::new(0));
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(NoRestartEngine {
            init_count: Rc::clone(&init_count),
            artifact: two_page_pdf_artifact(),
        });
        app.initialize().unwrap();
        assert_eq!(init_count.get(), 1);

        // The engine declares no rebuild needed (its `read_files` never
        // included this file); the CLI must short-circuit and not re-init.
        let messages = app
            .handle_editor_line(r#"(change "main.tex" 0 0 "%")"#)
            .unwrap();
        assert_eq!(
            init_count.get(),
            1,
            "a NoRestartNeeded edit must not rebuild"
        );
        assert!(
            messages.is_empty(),
            "no editor messages should be emitted for a skipped rebuild, got {messages:?}"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn pause_suppresses_rebuild_until_resume() {
        // Editors rely on: while paused, edits update the VFS but must NOT
        // rebuild; the next `resume` re-initializes the engine and picks them
        // up. Counting engine initializations to prove exactly one rebuild on
        // resume and none while paused.
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}x\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let init_count = Rc::new(Cell::new(0));
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(CountingArtifactEngine {
            init_count: Rc::clone(&init_count),
            artifact: two_page_pdf_artifact(),
        });
        app.initialize().unwrap();
        assert_eq!(init_count.get(), 1);

        // Pause: no rebuild.
        app.handle_editor_line("(pause)").unwrap();
        assert_eq!(init_count.get(), 1, "pause itself must not rebuild");

        // Edit while paused: VFS updates, engine is NOT re-initialized.
        app.handle_editor_line(r#"(change "main.tex" 0 0 "%")"#)
            .unwrap();
        assert_eq!(
            init_count.get(),
            1,
            "edits while paused must not rebuild the engine"
        );

        // Resume: exactly one rebuild, now picking up the edit.
        app.handle_editor_line("(resume)").unwrap();
        assert_eq!(
            init_count.get(),
            2,
            "resume must re-initialize once to apply deferred edits"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn rescan_forces_a_full_rebuild_without_a_vfs_change() {
        // `rescan` is the editor's "something on disk changed, redo everything"
        // signal: it must trigger exactly one full rebuild even when no tracked
        // file reported a change.
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}x\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file,
        };
        let root = root_document(&options).unwrap();
        let init_count = Rc::new(Cell::new(0));
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(CountingArtifactEngine {
            init_count: Rc::clone(&init_count),
            artifact: two_page_pdf_artifact(),
        });
        app.initialize().unwrap();
        assert_eq!(init_count.get(), 1);

        app.handle_editor_line("(rescan)").unwrap();
        assert_eq!(
            init_count.get(),
            2,
            "rescan must force exactly one full rebuild"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn rescan_reloads_external_disk_edits_to_the_root() {
        // TeXpresso rescan means "changed on disk, redo everything": a file
        // edited outside the editor open/change stream must be picked up.
        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}ORIGINAL\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        app.engine = Box::new(CountingArtifactEngine {
            init_count: Rc::new(Cell::new(0)),
            artifact: two_page_pdf_artifact(),
        });
        app.initialize().unwrap();
        let before = String::from_utf8_lossy(
            app.vfs
                .lookup("main.tex")
                .unwrap()
                .edit_data
                .as_deref()
                .unwrap(),
        )
        .to_string();
        assert!(before.contains("ORIGINAL"));
        fs::write(&root_file, "\\begin{document}EXTERNAL\\end{document}\n").unwrap();
        app.handle_editor_line("(rescan)").unwrap();
        let after = String::from_utf8_lossy(
            app.vfs
                .lookup("main.tex")
                .unwrap()
                .edit_data
                .as_deref()
                .unwrap(),
        )
        .to_string();
        assert!(
            after.contains("EXTERNAL"),
            "rescan must reload the root from disk"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    struct FailingDiagnosticEngine {
        diagnostics: Vec<Diagnostic>,
    }

    struct MissingLookupEngine {
        init_count: Rc<Cell<usize>>,
        target: String,
    }

    struct SynctexArtifactEngine {
        artifact: oxipresso_engine_api::DocumentArtifact,
        synctex: SyncTexArtifact,
    }

    impl TypesettingEngine for ArtifactEngine {
        fn initialize(
            &mut self,
            _root: &RootDocument,
            _io: &mut dyn EngineIo,
        ) -> oxipresso_engine_api::Result<EngineInit> {
            Ok(EngineInit {
                engine_name: "artifact-test".to_string(),
            })
        }

        fn step(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<EngineEvent> {
            Ok(EngineEvent::Idle)
        }

        fn apply_change_hint(
            &mut self,
            _changed_file: &PathId,
            _byte_offset: usize,
        ) -> oxipresso_engine_api::Result<RestartPolicy> {
            Ok(RestartPolicy::FullRestartRequired)
        }

        fn restart(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<()> {
            Ok(())
        }

        fn output_document(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
            Some(self.artifact.clone())
        }

        fn diagnostics(&self) -> &[Diagnostic] {
            &[]
        }
    }

    impl TypesettingEngine for CountingArtifactEngine {
        fn initialize(
            &mut self,
            _root: &RootDocument,
            _io: &mut dyn EngineIo,
        ) -> oxipresso_engine_api::Result<EngineInit> {
            self.init_count.set(self.init_count.get() + 1);
            Ok(EngineInit {
                engine_name: "counting-artifact-test".to_string(),
            })
        }

        fn step(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<EngineEvent> {
            Ok(EngineEvent::Idle)
        }

        fn apply_change_hint(
            &mut self,
            _changed_file: &PathId,
            _byte_offset: usize,
        ) -> oxipresso_engine_api::Result<RestartPolicy> {
            Ok(RestartPolicy::FullRestartRequired)
        }

        fn restart(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<()> {
            Ok(())
        }

        fn output_document(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
            Some(self.artifact.clone())
        }

        fn diagnostics(&self) -> &[Diagnostic] {
            &[]
        }
    }

    impl TypesettingEngine for FailingDiagnosticEngine {
        fn initialize(
            &mut self,
            _root: &RootDocument,
            io: &mut dyn EngineIo,
        ) -> oxipresso_engine_api::Result<EngineInit> {
            if let Ok(oxipresso_engine_api::OpenResult::Opened { handle, .. }) =
                io.open_read("main.tex", oxipresso_engine_api::FileKind::Tex)
            {
                io.close(handle)?;
            }
            Err(EngineError::new("engine failed"))
        }

        fn step(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<EngineEvent> {
            Ok(EngineEvent::Terminated)
        }

        fn apply_change_hint(
            &mut self,
            _changed_file: &PathId,
            _byte_offset: usize,
        ) -> oxipresso_engine_api::Result<RestartPolicy> {
            Ok(RestartPolicy::FullRestartRequired)
        }

        fn restart(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<()> {
            Err(EngineError::new("engine failed"))
        }

        fn output_document(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
            None
        }

        fn diagnostics(&self) -> &[Diagnostic] {
            &self.diagnostics
        }
    }

    impl TypesettingEngine for MissingLookupEngine {
        fn initialize(
            &mut self,
            _root: &RootDocument,
            io: &mut dyn EngineIo,
        ) -> oxipresso_engine_api::Result<EngineInit> {
            self.init_count.set(self.init_count.get() + 1);
            if let OpenResult::Opened { handle, .. } = io.open_read("main.tex", FileKind::Tex)? {
                io.close(handle)?;
            }
            if let OpenResult::Opened { handle, .. } = io.open_read(&self.target, FileKind::Tex)? {
                let _ = io.read(handle, 0, 1024)?;
                io.close(handle)?;
            }
            Ok(EngineInit {
                engine_name: "missing-lookup-test".to_string(),
            })
        }

        fn step(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<EngineEvent> {
            Ok(EngineEvent::Idle)
        }

        fn apply_change_hint(
            &mut self,
            _changed_file: &PathId,
            _byte_offset: usize,
        ) -> oxipresso_engine_api::Result<RestartPolicy> {
            Ok(RestartPolicy::FullRestartRequired)
        }

        fn restart(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<()> {
            Ok(())
        }

        fn output_document(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
            None
        }

        fn diagnostics(&self) -> &[Diagnostic] {
            &[]
        }
    }

    impl TypesettingEngine for SynctexArtifactEngine {
        fn initialize(
            &mut self,
            _root: &RootDocument,
            _io: &mut dyn EngineIo,
        ) -> oxipresso_engine_api::Result<EngineInit> {
            Ok(EngineInit {
                engine_name: "synctex-artifact-test".to_string(),
            })
        }

        fn step(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<EngineEvent> {
            Ok(EngineEvent::Idle)
        }

        fn apply_change_hint(
            &mut self,
            _changed_file: &PathId,
            _byte_offset: usize,
        ) -> oxipresso_engine_api::Result<RestartPolicy> {
            Ok(RestartPolicy::FullRestartRequired)
        }

        fn restart(&mut self, _io: &mut dyn EngineIo) -> oxipresso_engine_api::Result<()> {
            Ok(())
        }

        fn output_document(&self) -> Option<oxipresso_engine_api::DocumentArtifact> {
            Some(self.artifact.clone())
        }

        fn output_synctex(&self) -> Option<SyncTexArtifact> {
            Some(self.synctex.clone())
        }

        fn diagnostics(&self) -> &[Diagnostic] {
            &[]
        }
    }

    fn two_page_pdf_artifact() -> oxipresso_engine_api::DocumentArtifact {
        oxipresso_engine_api::DocumentArtifact {
            kind: ArtifactKind::Pdf,
            bytes: br#"%PDF-1.7
1 0 obj
<< /Type /Pages /Count 2 /Kids [2 0 R 3 0 R] >>
endobj
2 0 obj
<< /Type /Page /Parent 1 0 R >>
endobj
3 0 obj
<< /Type /Page /Parent 1 0 R >>
endobj
"#
            .to_vec(),
            source_name: Some("main.pdf".to_string()),
        }
    }

    fn unique_temp_dir() -> PathBuf {
        // nanos alone can collide between tests started in the same clock
        // tick (Windows timers are coarse); combine with a monotonic per-process
        // counter so concurrently-running tests never share/remove a dir.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "oxipresso-cli-test-{}-{nonce}-{seq}",
            std::process::id()
        ))
    }

    /// The process environment is global state. Tests that set `OXIPRESSO_ENGINE`
    /// (to exercise the external backend) must not overlap with tests whose
    /// assertions depend on the ambient/default engine, or a parallel run picks
    /// the wrong backend. `run_with_io` and `OxipressoApp::new` both consult
    /// `OXIPRESSO_ENGINE` via `choose_engine`, so every env-sensitive test
    /// serializes on this lock.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn restore_env_var(key: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }

    /// Protocol snapshot over the real engine (gated: real mode, an existing
    /// format file, and kpsewhich). Covers the initialization and
    /// change-rebuild message sequences: stream truncates, out/log appends,
    /// flushes, input-file and lookup-file notifications, and a parsed
    /// SyncTeX document for forward search.
    #[test]
    fn real_engine_protocol_snapshot() {
        if std::env::var("OXIPRESSO_USE_REAL_XETEX").ok().as_deref() != Some("1") {
            return;
        }
        if std::env::var_os("TEXPRESSO_SRC").is_none() {
            return;
        }
        let Some(format_path) = std::env::var_os("OXIPRESSO_XETEX_FORMAT") else {
            return;
        };
        if !std::path::Path::new(&format_path).is_file() {
            return;
        }
        let kpsewhich_available = std::process::Command::new("kpsewhich")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !kpsewhich_available {
            return;
        }

        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nSnapshot\n\\end{document}\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);

        // Initialization sequence.
        let init = app.initialize().unwrap();
        assert!(matches!(
            init.first(),
            Some(EditorMessage::Truncate {
                buffer: InfoBuffer::Out,
                size: 0
            })
        ));
        assert!(matches!(
            init.get(1),
            Some(EditorMessage::Truncate {
                buffer: InfoBuffer::Log,
                size: 0
            })
        ));
        let out_text: String = init
            .iter()
            .filter_map(|message| match message {
                EditorMessage::Append {
                    buffer: InfoBuffer::Out,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            out_text.to_lowercase().contains("xetex"),
            "out stream should contain the engine banner, got {out_text:?}"
        );
        let log_text: String = init
            .iter()
            .filter_map(|message| match message {
                EditorMessage::Append {
                    buffer: InfoBuffer::Log,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            log_text.to_lowercase().contains("xetex"),
            "log stream should contain the engine banner, got {log_text:?}"
        );
        assert!(init.iter().any(|message| matches!(
            message,
            EditorMessage::Append {
                buffer: InfoBuffer::Log,
                ..
            }
        )));
        assert!(
            init.iter()
                .any(|message| matches!(message, EditorMessage::Flush))
        );
        assert!(init.iter().any(|message| matches!(
            message,
            EditorMessage::InputFile { path, .. } if path == "main.tex"
        )));
        assert!(init.iter().any(|message| matches!(
            message,
            EditorMessage::LookupFile {
                path,
                status: oxipresso_editor_protocol::LookupStatus::Successful,
                ..
            } if path == "main.tex"
        )));
        assert!(app.synctex.is_some(), "SyncTeX sidecar should be parsed");

        // Reverse SyncTeX over the editor wire: the app maps viewer points
        // (points) on a real page through reverse search to a source location
        // and produces the engine-to-editor `synctex` notification. Sampling a
        // few page-1 points keeps this content-independent.
        let reversed = [
            app.synctex_reverse_message(1, 200.0, 120.0),
            app.synctex_reverse_message(1, 120.0, 200.0),
            app.synctex_reverse_message(1, 60.0, 60.0),
        ]
        .into_iter()
        .flatten()
        .next();
        assert!(
            matches!(reversed, Some(EditorMessage::Synctex { .. })),
            "reverse search on the real page 1 should map a point to a source location"
        );
        if let Some(EditorMessage::Synctex { path, line, .. }) = &reversed {
            assert!(
                !path.is_empty() && *line > 0,
                "reverse hit should name a real source location, got {path}:{line}"
            );
        }

        // Editor change -> rebuild sequence: channels truncate again and the
        // engine re-echoes the file open through the out channel.
        let rebuild = app
            .handle_editor_line("(change \"main.tex\" 41 8 \"Edited\")")
            .unwrap();
        assert!(matches!(
            rebuild.first(),
            Some(EditorMessage::Truncate {
                buffer: InfoBuffer::Out,
                size: 0
            })
        ));
        let rebuild_out: String = rebuild
            .iter()
            .filter_map(|message| match message {
                EditorMessage::Append {
                    buffer: InfoBuffer::Out,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            rebuild_out.contains("(main.tex"),
            "rebuild should re-echo the file open, got {rebuild_out:?}"
        );
        assert!(
            rebuild
                .iter()
                .any(|message| matches!(message, EditorMessage::Flush))
        );

        // Wire-format snapshot for the stream truncates.
        assert_eq!(
            serialize_message(
                &EditorMessage::Truncate {
                    buffer: InfoBuffer::Out,
                    size: 0
                },
                WireProtocol::Sexp
            ),
            "(truncate out 0)"
        );
        assert_eq!(
            serialize_message(
                &EditorMessage::Truncate {
                    buffer: InfoBuffer::Log,
                    size: 0
                },
                WireProtocol::Json
            ),
            "[\"truncate\",\"log\",0]"
        );
        fs::remove_dir_all(temp_dir).unwrap();
    }

    /// Resident CLI flow (gated like the protocol snapshot): with
    /// OXIPRESSO_RESIDENT=1 the app starts a persistent resident session;
    /// the initialization typesets pass 1 through the session, and every
    /// editor change becomes ONE hot pass (checkpoint restore + re-typeset,
    /// no format reload). The stream messages per pass keep the
    /// truncate/append/flush shape, and consecutive hot rebuilds prove the
    /// session persists across changes.
    #[test]
    fn real_engine_resident_cli_hot_rebuild() {
        if std::env::var("OXIPRESSO_USE_REAL_XETEX").ok().as_deref() != Some("1") {
            return;
        }
        if std::env::var_os("TEXPRESSO_SRC").is_none() {
            return;
        }
        let Some(format_path) = std::env::var_os("OXIPRESSO_XETEX_FORMAT") else {
            return;
        };
        if !std::path::Path::new(&format_path).is_file() {
            return;
        }
        let kpsewhich_available = std::process::Command::new("kpsewhich")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !kpsewhich_available {
            return;
        }
        // SAFETY: test-only env toggle; the CLI test binaries do not read
        // OXIPRESSO_RESIDENT from other threads concurrently.
        unsafe {
            std::env::set_var("OXIPRESSO_RESIDENT", "1");
        }

        let temp_dir = unique_temp_dir();
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(
            &root_file,
            "\\documentclass{article}\n\\begin{document}\nResident\n\\end{document}\n",
        )
        .unwrap();

        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);

        let init = app.initialize().unwrap();
        assert!(
            matches!(
                init.first(),
                Some(EditorMessage::Truncate {
                    buffer: InfoBuffer::Out,
                    size: 0
                })
            ),
            "resident pass 1 stream starts with the out truncate, got {init:?}"
        );
        assert!(
            init.iter()
                .any(|message| matches!(message, EditorMessage::Flush)),
            "resident pass 1 stream ends with a flush"
        );
        // Slice 2: the worker VFS's file events reach the wire — the root
        // open shows up as a successful lookup + input-file notification.
        assert!(init.iter().any(|message| matches!(
            message,
            EditorMessage::InputFile { path, .. } if path == "main.tex"
        )));
        assert!(init.iter().any(|message| matches!(
            message,
            EditorMessage::LookupFile {
                path,
                status: oxipresso_editor_protocol::LookupStatus::Successful,
                ..
            } if path == "main.tex"
        )));

        // Hot rebuild 1: the change is one pass through the live session.
        let rebuild1 = app
            .handle_editor_line("(change \"main.tex\" 41 8 \"Edited\")")
            .unwrap();
        assert!(
            matches!(
                rebuild1.first(),
                Some(EditorMessage::Truncate {
                    buffer: InfoBuffer::Out,
                    size: 0
                })
            ),
            "hot pass stream starts with the out truncate, got {rebuild1:?}"
        );
        let rebuild1_log: String = rebuild1
            .iter()
            .filter_map(|message| match message {
                EditorMessage::Append {
                    buffer: InfoBuffer::Log,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            rebuild1_log.to_lowercase().contains("xetex"),
            "hot pass log should carry the engine banner, got {rebuild1_log:?}"
        );

        // Hot rebuild 2: the session persists across changes (a full restart
        // would also pass this shape, but only the resident path keeps the
        // session alive without re-loading the format).
        let rebuild2 = app
            .handle_editor_line("(change \"main.tex\" 41 6 \"Again\")")
            .unwrap();
        assert!(
            rebuild2
                .iter()
                .any(|message| matches!(message, EditorMessage::Flush))
        );

        // read_files skip (P0.3 slice 2): register a file the engine NEVER
        // reads, then edit it — no hot pass may fire because the output
        // cannot change. "hello" is 5 bytes.
        app.handle_editor_line("(open-base64 \"unrelated.txt\" \"aGVsbG8=\")")
            .unwrap();
        let skipped = app
            .handle_editor_line("(change \"unrelated.txt\" 0 5 \"XYZW\")")
            .unwrap();
        assert!(
            skipped.is_empty(),
            "an edit to a file outside the pass read set must not trigger a rebuild, got {skipped:?}"
        );

        // The engine-read root still rebuilds after the skip (the read set
        // did not swallow relevant changes).
        let rebuild3 = app
            .handle_editor_line("(change \"main.tex\" 41 5 \"Final\")")
            .unwrap();
        assert!(
            rebuild3
                .iter()
                .any(|message| matches!(message, EditorMessage::Flush))
        );

        // SAFETY: paired with the set_var above.
        unsafe {
            std::env::remove_var("OXIPRESSO_RESIDENT");
        }
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn unknown_or_malformed_editor_command_is_skipped_not_fatal() {
        // TeXpresso (src/frontend/editor.c) reports bad commands to stderr and
        // keeps running. The replica must tolerate them too, not abort the
        // editor session on a single unparseable line.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("oxi-cli-tolerate-{nonce}"));
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}x\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        for bad in [
            "(frobnicate 1 2)",
            "(open)",
            "(open \"a\" \"b\" \"c\")",
            "not a command",
            "(((",
            "(change \"a\" x y z)",
        ] {
            let messages = app
                .handle_editor_line(bad)
                .unwrap_or_else(|e| panic!("session aborted on {bad:?}: {e}"));
            assert!(messages.is_empty(), "bad input {bad:?} produced messages");
        }
        // The session is still alive after the bad lines: a valid command works.
        assert!(app.handle_editor_line("(pause)").is_ok());
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn semantically_invalid_change_is_skipped_not_fatal() {
        // TeXpresso tolerates a well-formed but semantically invalid change
        // (bad line / not-open file / bad base64) by printing "skipping" and
        // continuing. A stale `(change-lines ...)` must not kill the session.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("oxi-cli-badchange-{nonce}"));
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}x\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        // change-lines to a file that is not open in the VFS -> Err internally,
        // must be tolerated (Ok) rather than aborting.
        app.handle_editor_line(r#"(change-lines "ghost.tex" 5 1 "x")"#)
            .expect("invalid change must be skipped, not fatal");
        // change with a byte range past EOF on the root file -> tolerated.
        app.handle_editor_line(r#"(change "main.tex" 99999 5 "x")"#)
            .expect("out-of-range change must be skipped, not fatal");
        // Session still alive.
        assert!(app.handle_editor_line("(pause)").is_ok());
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn theme_command_sets_themed_and_applies_colors() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("oxi-cli-theme-{nonce}"));
        fs::create_dir_all(&temp_dir).unwrap();
        let root_file = temp_dir.join("main.tex");
        fs::write(&root_file, "\\begin{document}x\\end{document}\n").unwrap();
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: false,
            gui: false,
            root_file: root_file.clone(),
        };
        let root = root_document(&options).unwrap();
        let mut app = OxipressoApp::new(options, root);
        // Must not panic; must mark the viewer themed (colors converted + sent
        // to the renderer). The pixel-level effect is tested in oxipresso-render.
        app.handle_editor_line(r"(theme (0.0 0.0 1.0) (1.0 0.0 0.0))")
            .unwrap();
        assert!(app.viewer_state().themed, "theme command should set themed");
        fs::remove_dir_all(temp_dir).unwrap();
    }
}
