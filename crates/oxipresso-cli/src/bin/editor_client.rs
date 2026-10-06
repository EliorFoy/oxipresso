//! The Oxipresso live editor client (P1): a TeXpresso-style CLIENT that
//! drives the `oxipresso` render server over the wire. The server runs as a
//! child process in `-stream` mode (files are pushed by the client, exactly
//! like an editor plugin pushes unsaved buffers); the client owns a text
//! editor pane — every edit is debounced and pushed as a whole-buffer
//! `(change ...)`, giving the real-time "type and see" loop — and displays
//! the rendered page plus the engine's log stream.
//!
//! Threading (the freeze-proof design): the UI thread NEVER touches the
//! child pipes. Three workers own the blocking ends:
//! - the WIRE worker owns `EditorWireSession`: sends queued commands and
//!   drains notices into `UiEvent::Log` (throttled; a CN first pass floods
//!   thousands of lines);
//! - the RENDER worker owns a dedicated glyph backend and ships
//!   `UiEvent::Page` pixel buffers;
//! - the Slint timer on the UI thread adopts finished events and hands the
//!   wire worker new commands through an unbounded channel — a slow child
//!   (a 24KB change while a pass runs) can never block the UI.

use oxipresso_cli::KpseFontResolver;
use oxipresso_cli::editor_wire::{EditorWireSession, WireNotice, escape_wire_string};
use oxipresso_editor_protocol::{InfoBuffer, WireProtocol};
use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_render::{RenderBackend, XdvGlyphRenderBackend};
use slint::{ComponentHandle, SharedString};
use std::sync::mpsc::TryRecvError;

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
                Text { text: "engine log (tail)"; font-size: 11px; }
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

/// Plain-data events from the workers to the UI timer (all Send).
enum UiEvent {
    Page(SharedPixelBuffer, String),
    Log(String),
}

/// Sendable pixel buffer for the preview pane (slint::Image is not Send;
/// the UI thread wraps it at display time).
struct SharedPixelBuffer {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
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
    // Double-click launch (no arguments): pick a document with the native
    // file dialog instead of exiting with a usage message that flashes
    // away in a console.
    let doc = match doc {
        Some(path) => Some(path),
        None => rfd::FileDialog::new()
            .add_filter("TeX documents", &["tex", "sty", "cls", "ltx"])
            .set_title("Open a TeX document to preview")
            .pick_file(),
    };
    let Some(doc_path) = doc else {
        return; // the user cancelled the dialog
    };
    let doc_path = doc_path.canonicalize().unwrap_or_else(|_| doc_path.clone());
    let wire_name = doc_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| doc_path.to_string_lossy().into_owned());
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

    let (wire_cmd_tx, wire_cmd_rx) = std::sync::mpsc::channel::<String>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    // Due-edit texts: the UI thread hands the current buffer to the wire
    // worker, which performs the possibly-blocking whole-buffer send.
    let (edit_due_tx, edit_due_rx) = std::sync::mpsc::channel::<String>();

    // Stream-mode init dance: register → open (unsaved buffer) → resume.
    {
        use base64::Engine as _;
        let initial_b64 = base64::engine::general_purpose::STANDARD.encode(&initial);
        for line in [
            format!("(register {})", escape_wire_string(&wire_name)),
            format!(
                "(open-base64 {} \"{initial_b64}\")",
                escape_wire_string(&wire_name)
            ),
            "(resume)".to_string(),
        ] {
            let _ = wire_cmd_tx.send(line);
        }
    }

    // The WIRE worker: owns the session. Sends queued commands in order
    // (a 24KB change while a pass runs blocks THIS thread, never the UI),
    // drains notices into the log tail, and forwards everything to the UI.
    {
        let ui_tx = ui_tx.clone();
        std::thread::spawn(move || {
            let mut session = session;
            let mut log = String::new();
            let mut last_log_send = std::time::Instant::now();
            // The engine buffer starts as the opened document: the first
            // edit replaces exactly those bytes.
            let mut last_sent_len: Option<usize> = Some(initial.len());
            let mut out_buffer = String::new();
            loop {
                // 1. Send every queued command in order (changes are
                //    cumulative; dropping one would desync the buffer).
                loop {
                    match wire_cmd_rx.try_recv() {
                        Ok(line) => {
                            if let Err(error) = session.send_raw(&line) {
                                eprintln!("[client] {error}");
                            }
                            if let Ok(log_path) = std::env::var("OXI_CLIENT_LOG") {
                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(log_path)
                                {
                                    use std::io::Write as _;
                                    let _ = writeln!(f, "SEND {line}");
                                }
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                // 2. Apply due edit flushes: the UI thread hands over the
                //    current buffer text; compute old_len here.
                while let Ok(new_text) = edit_due_rx.try_recv() {
                    let old_len = last_sent_len.unwrap_or(new_text.len());
                    last_sent_len = Some(new_text.len());
                    let line = format!(
                        "(change {} 0 {old_len} {})",
                        escape_wire_string(&wire_name),
                        escape_wire_string(&new_text)
                    );
                    if let Err(error) = session.send_raw(&line) {
                        eprintln!("[client] {error}");
                    }
                }
                // 3. Drain notices into the log tail.
                loop {
                    let Some(parsed) = session.next_notice(std::time::Duration::ZERO) else {
                        break;
                    };
                    match &parsed.notice {
                        WireNotice::Truncate {
                            buffer: InfoBuffer::Out,
                            ..
                        } => out_buffer.clear(),
                        WireNotice::Append {
                            buffer: InfoBuffer::Out,
                            pos: Some(pos),
                            text,
                            ..
                        } => {
                            let start = (*pos).min(out_buffer.len());
                            out_buffer.insert_str(start, text);
                        }
                        WireNotice::Append { .. } => {}
                        _ => {}
                    }
                    if log.len() > 96 * 1024 {
                        log = log[log.len() - 64 * 1024..].to_string();
                    }
                    log.push_str(&parsed.raw);
                    log.push('\n');
                }
                // 4. Forward the log tail at most ~3x/second (the CN first
                //    pass floods thousands of notices).
                if last_log_send.elapsed() >= std::time::Duration::from_millis(350) {
                    last_log_send = std::time::Instant::now();
                    let view = if log.len() > 6 * 1024 {
                        &log[log.len() - 6 * 1024..]
                    } else {
                        log.as_str()
                    };
                    let _ = ui_tx.send(UiEvent::Log(view.to_string()));
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        });
    }

    // The RENDER worker: dedicated glyph backend, artifact watching, pixel
    // buffers to the UI.
    {
        let artifact_path = artifact_path.clone();
        std::thread::spawn(move || {
            let backend = XdvGlyphRenderBackend::new(Box::new(
                KpseFontResolver::detect().unwrap_or_else(|| KpseFontResolver::dummy()),
            ));
            let mut last_len: Option<usize> = None;
            loop {
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
                        let _ =
                            ui_tx.send(UiEvent::Page(buffer, format!("rendered {size} B of XDV")));
                    }
                    Err(error) => {
                        let _ = ui_tx.send(UiEvent::Page(
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

    // UI thread: adopt worker events into the panes.
    let timer = slint::Timer::default();
    {
        let ui_handle = ui.clone_strong();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(120),
            move || {
                let mut log_tail: Option<String> = None;
                let mut page_event: Option<(SharedPixelBuffer, String)> = None;
                while let Ok(event) = ui_rx.try_recv() {
                    match event {
                        UiEvent::Log(tail) => log_tail = Some(tail),
                        UiEvent::Page(buffer, status) => page_event = Some((buffer, status)),
                    }
                }
                if let Some(tail) = log_tail {
                    ui_handle.set_engine_log(SharedString::from(tail.as_str()));
                }
                if let Some((buffer, status)) = page_event {
                    if buffer.width > 0 {
                        let image =
                            slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                                &buffer.rgba,
                                buffer.width,
                                buffer.height,
                            ));
                        ui_handle.set_page_image(image);
                    }
                    ui_handle.set_status(SharedString::from(status));
                }
            },
        );
    }

    // Editor edits: the due-buffer text goes straight to the wire worker
    // (it performs the debounced, possibly-blocking whole-buffer send).
    {
        let ui_weak = ui.as_weak();
        let edit_due_tx = edit_due_tx.clone();
        ui.on_editor_edited(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = edit_due_tx.send(ui.get_document_text().to_string());
            }
        });
    }

    ui.run().expect("slint event loop");
}
