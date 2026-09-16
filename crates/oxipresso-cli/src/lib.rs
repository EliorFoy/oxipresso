use std::{
    io::{BufRead, Write},
    path::{Path, PathBuf},
};

use oxipresso_editor_protocol::{
    EditorCommand, EditorMessage, InfoBuffer, WireProtocol, parse_command, serialize_message,
};
use oxipresso_engine_api::{RestartPolicy, RootDocument, TypesettingEngine};
use oxipresso_engine_external::ExternalEngine;
use oxipresso_engine_xetex::XetexEngine;
use oxipresso_render::AutoRenderBackend;
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
        root_file: root_file.ok_or_else(|| "missing root TeX document".to_string())?,
    })
}

pub fn run_with_io<R, W>(options: CliOptions, input: R, mut output: W) -> Result<(), String>
where
    R: BufRead,
    W: Write,
{
    let root = root_document(&options)?;
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

pub struct OxipressoApp {
    options: CliOptions,
    root: RootDocument,
    vfs: VirtualFileSystem,
    engine: Box<dyn TypesettingEngine>,
    viewer: ViewerState,
    renderer: AutoRenderBackend,
    synctex: Option<SyncTexDocument>,
    paused: bool,
}

impl OxipressoApp {
    pub fn new(options: CliOptions, root: RootDocument) -> Self {
        let paused = options.stream_mode;
        let mut vfs = VirtualFileSystem::new();
        vfs.set_disk_roots(disk_roots_for(&root));
        // TeX Live provider: resolve distribution files (format sources,
        // classes, fonts) through kpsewhich when available, mirroring the
        // original engine's texlive backend. Editor buffers and disk roots
        // still take precedence.
        if let Some(resolver) = oxipresso_engine_xetex::texlive::KpsewhichResolver::auto() {
            vfs.set_resolver(Box::new(resolver));
        }
        Self {
            options,
            root,
            vfs,
            engine: choose_engine(),
            viewer: ViewerState::default(),
            renderer: AutoRenderBackend::default(),
            synctex: None,
            paused,
        }
    }

    pub fn viewer_state(&self) -> &ViewerState {
        &self.viewer
    }

    pub fn initialize(&mut self) -> Result<Vec<EditorMessage>, String> {
        let mut messages = Vec::new();
        if !self.paused {
            self.prime_root_from_disk()?;
            messages.extend(self.initialize_engine()?);
        }
        messages.extend(self.drain_input_messages());
        messages.extend(self.drain_lookup_messages());
        Ok(messages)
    }

    pub fn handle_editor_line(&mut self, line: &str) -> Result<Vec<EditorMessage>, String> {
        let command = parse_command(line, self.options.protocol).map_err(|e| e.to_string())?;
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
                messages.extend(self.initialize_engine()?);
                messages.extend(self.drain_input_messages());
                messages.extend(self.drain_lookup_messages());
                return Ok(messages);
            }
            EditorCommand::PreviousPage => self.viewer.previous_page(),
            EditorCommand::NextPage => self.viewer.next_page(),
            EditorCommand::Crop => self.viewer.toggle_crop(),
            EditorCommand::Invert => self.viewer.toggle_invert(),
            EditorCommand::Theme { .. } => self.viewer.themed = true,
            EditorCommand::SynctexForward { path, line } => self.apply_synctex_forward(path, *line),
            _ => {}
        }

        let outcome = self
            .vfs
            .apply_editor_command(&command)
            .map_err(|e| e.to_string())?;
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
        let mut messages = self.engine_output_messages();
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
        EngineIo, FileKind, OpenResult, PathId, RestartPolicy, SyncTexArtifact,
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
            "-test-initialize",
            "main.tex",
        ])
        .unwrap();
        assert_eq!(opts.protocol, WireProtocol::Json);
        assert!(opts.line_output);
        assert!(opts.stream_mode);
        assert!(opts.initialize_only);
        assert_eq!(opts.include_paths, vec![PathBuf::from("build")]);
    }

    #[test]
    fn rejects_provider_conflict() {
        assert!(parse_args(["-texlive", "-tectonic", "main.tex"]).is_err());
    }

    #[test]
    fn register_does_not_emit_until_engine_lookup() {
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: true,
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
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: true,
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
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: true,
            stream_mode: true,
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
        let options = CliOptions {
            include_paths: Vec::new(),
            protocol: WireProtocol::Sexp,
            line_output: false,
            provider: PackageProvider::Auto,
            initialize_only: false,
            stream_mode: true,
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
            root_file,
        };
        let mut output = Vec::new();
        run_with_io(options, std::io::Cursor::new(Vec::<u8>::new()), &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(r#"(input-file 0 "main.tex")"#));
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn json_protocol_initialization_serializes_messages_as_json() {
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
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oxipresso-cli-test-{nonce}"))
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
}
