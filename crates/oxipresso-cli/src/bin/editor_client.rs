//! The Oxipresso live editor client (P1): a TeXpresso-style CLIENT that
//! drives the `oxipresso` render server over the wire. The server runs as a
//! child process in `-stream` mode (files are pushed by the client, exactly
//! like an editor plugin pushes unsaved buffers); the client owns a text
//! editor pane — every edit is debounced and pushed as a whole-buffer
//! `(change ...)`, giving the real-time "type and see" loop — and displays
//! the rendered page plus the engine's log stream.
//!
//! Pipeline: client edit → wire change → engine re-typeset (resident hot
//! pass) → XDV artifact (OXIPRESSO_ARTIFACT_OUT) → the client's artifact
//! watcher renders page 1 with the glyph backend into the preview pane.
//!
//! Threading: `ClientCore` lives on the UI thread only (the wire session is
//! mutex-guarded; the Slint window is not Send). The render thread owns a
//! DEDICATED glyph backend (its caches are thread-local) and delivers plain
//! `UiEvent`s over a channel; the Slint timer on the UI thread drains both
//! the wire notices and the render events.

use oxipresso_cli::KpseFontResolver;
use oxipresso_cli::editor_wire::{EditorWireSession, WireNotice, escape_wire_string};
use oxipresso_editor_protocol::{InfoBuffer, WireProtocol};
use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_render::{RenderBackend, XdvGlyphRenderBackend};
use slint::{ComponentHandle, SharedString};

slint::slint! {
    import { Button, TextEdit } from "std-widgets.slint";

    export component EditorClientWindow inherits Window {
        title: "Oxipresso editor client";
        preferred-width: 1400px;
        preferred-height: 900px;
        in property <string> doc-path;
        in-out property <string> document-text <=> doc-edit.text;
        in-out property <image> page-image;
        in-out property <string> engine-log <=> log-edit.text;
        in-out property <string> status;
        callback editor-edited();

        HorizontalLayout {
            padding: 8px;
            spacing: 8px;
            VerticalLayout {
                spacing: 6px;
                horizontal-stretch: 2;
                Text { text: "editor — " + doc-path + "  (edits push live)"; font-size: 11px; }
                doc-edit := TextEdit {
                    text: "";
                    vertical-stretch: 1;
                    font-size: 13px;
                    edited => { editor-edited(); }
                }
                Button { text: "Push now"; clicked => { editor-edited(); } }
            }
            VerticalLayout {
                spacing: 6px;
                horizontal-stretch: 3;
                Text { text: "rendered preview"; font-size: 11px; }
                page-view := Image {
                    source: page-image;
                    image-fit: contain;
                    vertical-stretch: 3;
                }
                Text { text: "engine log"; font-size: 11px; }
                log-edit := TextEdit {
                    text: "";
                    read-only: true;
                    vertical-stretch: 1;
                    font-size: 11px;
                }
                Text { text: status; font-size: 11px; color: gray; }
            }
        }
    }
}

/// Plain-data event from the render thread to the UI timer.
enum UiEvent {
    Page(SharedPixelBuffer, String),
}

/// Sendable pixel buffer for the preview pane (slint::Image is not Send;
/// the UI thread wraps it at display time).
struct SharedPixelBuffer {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

struct ClientCore {
    session: std::sync::Mutex<EditorWireSession>,
    doc_path: std::path::PathBuf,
    ui: EditorClientWindow,
    last_sent_len: std::cell::Cell<usize>,
    edit_due: std::cell::Cell<bool>,
    engine_log: std::cell::RefCell<String>,
    out_buffer: std::cell::RefCell<String>,
}

impl ClientCore {
    fn send_raw(&self, line: String) {
        let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(error) = guard.send_raw(&line) {
            eprintln!("[client] {error}");
        }
    }

    /// The editor typed: mark the whole-buffer push due (the timer performs
    /// the actual send, debounced to one rebuild per tick).
    fn mark_edit_due(&self) {
        self.edit_due.set(true);
    }

    /// Push the current editor buffer as a whole-buffer change (what an
    /// editor save produces): `(change path 0 old_len new_bytes)`.
    fn flush_edit(&self, new_text: &str) {
        if !self.edit_due.get() {
            return;
        }
        self.edit_due.set(false);
        let old_len = self.last_sent_len.get();
        self.last_sent_len.set(new_text.len());
        let line = format!(
            "(change {} 0 {old_len} {})",
            escape_wire_string(&self.doc_path.to_string_lossy()),
            escape_wire_string(new_text)
        );
        self.send_raw(line);
    }

    /// Drains pending engine notices into the log pane (UI thread).
    fn drain_notices(&self) {
        let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let Some(parsed) = guard.next_notice(std::time::Duration::ZERO) else {
                break;
            };
            match &parsed.notice {
                WireNotice::Truncate {
                    buffer: InfoBuffer::Out,
                    ..
                } => self.out_buffer.borrow_mut().clear(),
                WireNotice::Append {
                    buffer: InfoBuffer::Out,
                    pos: Some(pos),
                    text,
                    ..
                } => {
                    let mut buf = self.out_buffer.borrow_mut();
                    let start = (*pos).min(buf.len());
                    buf.insert_str(start, text);
                }
                WireNotice::Append { .. } => {}
                _ => {}
            }
            self.push_log_line(&parsed.raw);
        }
        self.refresh_log_pane();
    }

    fn push_log_line(&self, line: &str) {
        let mut log = self.engine_log.borrow_mut();
        if log.len() > 96 * 1024 {
            *log = log[log.len() - 64 * 1024..].to_string();
        }
        log.push_str(line);
        log.push('\n');
    }

    fn refresh_log_pane(&self) {
        let log = self.engine_log.borrow();
        self.ui.set_engine_log(SharedString::from(log.as_str()));
    }

    fn set_status(&self, text: &str) {
        self.ui.set_status(SharedString::from(text));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut binary: Option<std::path::PathBuf> = None;
    let mut doc: Option<std::path::PathBuf> = None;
    let mut json = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--binary" => binary = args.next().map(std::path::PathBuf::from),
            "--json" => json = true,
            other if doc.is_none() && !other.starts_with('-') => {
                doc = Some(std::path::PathBuf::from(other))
            }
            other => {
                eprintln!("oxipresso-editor-client: unexpected argument {other}");
                eprintln!("usage: oxipresso-editor-client [--binary PATH] [--json] document.tex");
                std::process::exit(2);
            }
        }
    }
    let Some(doc_path) = doc else {
        eprintln!("usage: oxipresso-editor-client [--binary PATH] [--json] document.tex");
        std::process::exit(2);
    };
    let doc_path = doc_path.canonicalize().unwrap_or_else(|_| doc_path.clone());
    let binary = binary.unwrap_or_else(|| {
        let exe = std::env::current_exe().expect("current exe");
        let sibling = exe.with_file_name(if cfg!(windows) {
            "oxipresso.exe"
        } else {
            "oxipresso"
        });
        if sibling.is_file() {
            sibling
        } else {
            eprintln!("oxipresso binary not found next to the client ({sibling:?}); pass --binary");
            std::process::exit(2);
        }
    });

    // The rendered XDV artifact lands here after every rebuild; the client
    // watches it and displays page 1.
    let artifact_path = doc_path.with_extension("xdv");
    // Child environment: the render server needs the format file and writes
    // the artifact for the client's preview pane (the child inherits this
    // process's environment).
    unsafe {
        std::env::set_var("OXIPRESSO_RESIDENT", "1");
        std::env::set_var("OXIPRESSO_ARTIFACT_OUT", &artifact_path);
    }

    let protocol = if json {
        WireProtocol::Json
    } else {
        WireProtocol::Sexp
    };
    let session = match EditorWireSession::spawn(&binary, &doc_path, protocol, true) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("oxipresso-editor-client: {error}");
            std::process::exit(1);
        }
    };

    let ui = EditorClientWindow::new().expect("slint window");
    ui.set_doc_path(doc_path.to_string_lossy().into_owned().into());
    let initial = std::fs::read(&doc_path).unwrap_or_default();
    ui.set_document_text(String::from_utf8_lossy(&initial).into_owned().into());
    ui.set_status("starting the engine...".into());

    let window = std::sync::Arc::new(ClientCore {
        session: std::sync::Mutex::new(session),
        doc_path: doc_path.clone(),
        ui: ui.clone_strong(),
        last_sent_len: std::cell::Cell::new(initial.len()),
        edit_due: std::cell::Cell::new(false),
        engine_log: std::cell::RefCell::new(String::new()),
        out_buffer: std::cell::RefCell::new(String::new()),
    });

    // Stream-mode init dance: register → open (unsaved buffer) → resume.
    {
        use base64::Engine as _;
        let core = std::sync::Arc::clone(&window);
        let initial_b64 = base64::engine::general_purpose::STANDARD.encode(&initial);
        core.send_raw(format!(
            "(register {})",
            escape_wire_string(&doc_path.to_string_lossy())
        ));
        core.send_raw(format!(
            "(open-base64 {} \"{initial_b64}\")",
            escape_wire_string(&doc_path.to_string_lossy())
        ));
        core.send_raw("(resume)".to_string());
    }

    // Editor edits: mark due (the timer performs the debounced push).
    {
        let core = std::sync::Arc::clone(&window);
        ui.on_editor_edited(move || core.mark_edit_due());
    }

    // Render events channel + the render thread with a DEDICATED backend.
    let (render_tx, render_rx) = std::sync::mpsc::channel::<UiEvent>();
    {
        let artifact_path = artifact_path.clone();
        std::thread::spawn(move || {
            let mut watcher = oxipresso_platform::file_watcher(artifact_path.clone());
            let backend = XdvGlyphRenderBackend::new(Box::new(
                KpseFontResolver::detect().unwrap_or_else(|| KpseFontResolver::dummy()),
            ));
            let mut last_len: Option<usize> = None;
            loop {
                // Pull-based watcher + size fingerprint: a rebuilt artifact
                // changes length; the watcher consumes the FS event.
                if let oxipresso_platform::FileWatchEvent::Changed { .. } = watcher.poll() {
                    watcher.mark_clean(None);
                }
                let len = std::fs::metadata(&artifact_path)
                    .ok()
                    .map(|m| m.len() as usize);
                if len.is_none() || len == last_len {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
                let Ok(bytes) = std::fs::read(&artifact_path) else {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                };
                last_len = Some(bytes.len());
                let artifact = DocumentArtifact {
                    kind: ArtifactKind::Xdv,
                    bytes,
                    source_name: Some(artifact_path.to_string_lossy().into_owned()),
                };
                match backend.render_page(&artifact, 0) {
                    Ok(page) => {
                        let buffer = SharedPixelBuffer {
                            rgba: page.pixels_rgba,
                            width: page.width,
                            height: page.height,
                        };
                        let size = artifact.bytes.len();
                        let _ = render_tx
                            .send(UiEvent::Page(buffer, format!("rendered {size} B of XDV")));
                    }
                    Err(error) => {
                        let _ = render_tx.send(UiEvent::Page(
                            SharedPixelBuffer {
                                rgba: Vec::new(),
                                width: 0,
                                height: 0,
                            },
                            format!("render error: {error}"),
                        ));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
        });
    }

    // UI timer: drain wire notices, drain render events, push due edits.
    let timer = slint::Timer::default();
    {
        let core = std::sync::Arc::clone(&window);
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(120),
            move || {
                core.drain_notices();
                loop {
                    match render_rx.try_recv() {
                        Ok(UiEvent::Page(buffer, status)) => {
                            let image = slint::Image::from_rgba8(
                                slint::SharedPixelBuffer::clone_from_slice(
                                    &buffer.rgba,
                                    buffer.width,
                                    buffer.height,
                                ),
                            );
                            core.ui.set_page_image(image);
                            core.set_status(&status);
                        }
                        Err(_) => break,
                    }
                }
                core.flush_edit(&core.ui.get_document_text().to_string());
            },
        );
    }

    ui.run().expect("slint event loop");
}
