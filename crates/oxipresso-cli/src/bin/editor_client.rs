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
//! - the WIRE worker owns `EditorWireSession`: sends the init dance, then
//!   COALESCES pending buffer texts into one whole-buffer change (typing
//!   bursts cost one hot pass, never a queue of rebuilds), and drains
//!   notices into `UiEvent::Log` (throttled);
//! - the RENDER worker owns a dedicated glyph backend and ships
//!   `UiEvent::Page` pixel buffers (keyed on len+mtime so equal-length
//!   rebuilds still update);
//! - the Slint timer on the UI thread adopts finished events — a slow child
//!   can never block the UI.

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
    let mut auto_edit: Option<String> = None;
    let mut log_path: Option<std::path::PathBuf> = None;
    let mut json = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--binary" => binary = args.next().map(std::path::PathBuf::from),
            "--auto-edit" => auto_edit = args.next(),
            "--log" => log_path = args.next().map(std::path::PathBuf::from),
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

    // edit_text: the UI hands the CURRENT buffer text; the wire worker
    // coalesces and pushes.
    let (edit_text_tx, edit_text_rx) = std::sync::mpsc::channel::<String>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();

    // The WIRE worker: owns the session. Sends the init dance, then
    // coalesces pending buffer texts into ONE whole-buffer change per
    // cycle (typing bursts cost a single hot pass), and drains notices
    // into the throttled log tail. A slow child blocks THIS thread only.
    {
        let wire_name = wire_name.clone();
        let initial_len = initial.len();
        let spawn_binary = binary.clone();
        let spawn_doc = doc_path.clone();
        let log_file = log_path.clone();
        let ui_tx = ui_tx.clone();
        std::thread::spawn(move || {
            let mut session = session;
            let mut log = String::new();
            let mut last_log_send = std::time::Instant::now();
            // The engine's buffer starts as the opened document: the first
            // edit replaces exactly those bytes.
            let mut engine_len: usize = initial_len;
            let mut pending_text: Option<String> = None;
            let mut last_known_text = String::from_utf8_lossy(&initial).into_owned();
            let mut out_buffer = String::new();
            let mut last_stderr_len = 0usize;

            let send_line = |session: &mut EditorWireSession, line: &str| -> bool {
                if let Err(error) = session.send_raw(line) {
                    eprintln!("[client] {error}");
                    return false;
                }
                true
            };
            let log_send = |line: &str| {
                if let Some(log_path) = &log_file {
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log_path)
                    {
                        use std::io::Write as _;
                        let _ = writeln!(f, "SEND {line}");
                    }
                }
            };

            // Init dance: register → open (unsaved buffer) → resume.
            {
                use base64::Engine as _;
                let initial_b64 = base64::engine::general_purpose::STANDARD.encode(&initial);
                send_line(
                    &mut session,
                    &format!("(register {})", escape_wire_string(&wire_name)),
                );
                send_line(
                    &mut session,
                    &format!(
                        "(open-base64 {} \"{initial_b64}\")",
                        escape_wire_string(&wire_name)
                    ),
                );
                send_line(&mut session, "(resume)");
            }

            loop {
                // 1. Coalesce pending buffer texts into ONE whole-buffer
                //    change: the latest state supersedes everything typed
                //    before it (a typing burst costs a single hot pass).
                while let Ok(text) = edit_text_rx.try_recv() {
                    pending_text = Some(text);
                }
                if let Some(text) = pending_text.take() {
                    let old_len = engine_len;
                    engine_len = text.len();
                    last_known_text = text.clone();
                    let line = format!(
                        "(change {} 0 {old_len} {})",
                        escape_wire_string(&wire_name),
                        escape_wire_string(&text)
                    );
                    log_send(&line);
                    if !send_line(&mut session, &line) {
                        // The child died (the hot pass crashed for this
                        // document): restart it and re-push the current
                        // buffer as a fresh open — slow but correct.
                        eprintln!("[client] engine died; respawning");
                        match EditorWireSession::spawn(&spawn_binary, &spawn_doc, protocol, true) {
                            Ok(new_session) => {
                                session = new_session;
                                use base64::Engine as _;
                                let b64 = base64::engine::general_purpose::STANDARD
                                    .encode(last_known_text.as_bytes());
                                send_line(
                                    &mut session,
                                    &format!("(register {})", escape_wire_string(&wire_name)),
                                );
                                send_line(
                                    &mut session,
                                    &format!(
                                        "(open-base64 {} \"{b64}\")",
                                        escape_wire_string(&wire_name)
                                    ),
                                );
                                send_line(&mut session, "(resume)");
                            }
                            Err(error) => {
                                eprintln!("[client] respawn failed: {error}");
                                return;
                            }
                        }
                    }
                }
                // 2. Drain notices into the log tail.
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
                // The child stderr rides along: an engine abort/panic lands here.
                let child_err = session.stderr_text();
                if child_err.len() != last_stderr_len {
                    last_stderr_len = child_err.len();
                    log.push_str(&child_err);
                    log.push('\n');
                }
                // 3. Forward the log tail at most ~3x/second (the CN first
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

    // The RENDER worker: dedicated glyph backend, artifact watching keyed on
    // (len, mtime) — an equal-length rebuild still updates — and pixel
    // buffers to the UI.
    {
        let artifact_path = artifact_path.clone();
        std::thread::spawn(move || {
            let backend = XdvGlyphRenderBackend::new(Box::new(
                KpseFontResolver::detect().unwrap_or_else(|| KpseFontResolver::dummy()),
            ));
            let mut fingerprint: Option<(usize, u64)> = None;
            loop {
                let meta = std::fs::metadata(&artifact_path).ok();
                let current = meta.as_ref().map(|m| {
                    (
                        m.len() as usize,
                        m.modified()
                            .ok()
                            .and_then(|t| Some(t.duration_since(std::time::UNIX_EPOCH).ok()?))
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0),
                    )
                });
                if current.is_none() || current == fingerprint {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    continue;
                }
                fingerprint = current;
                let Ok(bytes) = std::fs::read(&artifact_path) else {
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    continue;
                };
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

    // Editor edits and Push: hand the CURRENT buffer text to the wire
    // worker (it coalesces and performs the possibly-blocking send).
    {
        let ui_weak = ui.as_weak();
        let edit_text_tx = edit_text_tx.clone();
        ui.on_editor_edited(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = edit_text_tx.send(ui.get_document_text().to_string());
            }
        });
    }

    // OXI_CLIENT_AUTOEDIT=<text>: after the initial pass, programmatically
    // edit the buffer (the same path a keystroke takes: edited -> wire
    // worker -> change) — the live-edit regression test without OS input.
    if let Some(auto_text) = auto_edit {
        let ui_weak = ui.as_weak();
        let edit_text_tx = edit_text_tx.clone();
        let auto_timer = slint::Timer::default();
        auto_timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(3000),
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let current = ui.get_document_text().to_string();
                    // Insert a VISIBLE marker into the page-1 keywords line
                    // (the auto-edit must produce a visible page-1 change).
                    let edited =
                        current.replace("双树复小波变换", &format!("双树复小波变换 {}", auto_text));
                    let _ = edit_text_tx.send(edited);
                    ui.set_status(SharedString::from("auto-edit pushed"));
                }
            },
        );
        std::mem::forget(auto_timer);
    }

    ui.run().expect("slint event loop");
}
