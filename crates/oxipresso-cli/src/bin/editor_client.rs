//! The Oxipresso reference editor client (P1): a small Slint window wired to
//! a live `oxipresso` engine process over the editor wire. This is the
//! integration contract an emacs/vscode plugin implements, as a runnable
//! GUI:
//!
//! - the engine runs as a CHILD process; the client speaks the protocol over
//!   stdin/stdout exactly like TeXpresso's plugins do;
//! - the left pane edits the document; "Send change" issues the whole-buffer
//!   `(change path 0 old-len new)` an editor save produces;
//! - the right pane shows the engine's notices (truncate/append/flush,
//!   input-file/lookup-file) and the mirrored `out` info buffer, applied
//!   exactly the way a plugin's info buffers would be.
//!
//! Threading: the session reader thread owns the wire and the out-buffer
//! mirror and sends plain-data events over a channel; a repeating Slint
//! timer on the UI thread drains the channel (Slint components are not Send,
//! so UI access stays on the event loop).

use oxipresso_cli::editor_wire::{EditorWireSession, WireNotice, escape_wire_string};
use oxipresso_editor_protocol::{InfoBuffer, WireProtocol};
use slint::{ComponentHandle, SharedString};

slint::slint! {
    import { Button, TextEdit } from "std-widgets.slint";

    export component EditorClientWindow inherits Window {
        title: "Oxipresso editor client";
        preferred-width: 1080px;
        preferred-height: 720px;
        in property <string> doc-path;
        in-out property <string> document-text <=> doc-edit.text;
        in-out property <string> message-log <=> notice-log.text;
        in-out property <string> out-buffer <=> out-buffer-text.text;
        callback send-open();
        callback send-change();
        callback send-rescan();
        callback send-pause();
        callback send-resume();

        HorizontalLayout {
            padding: 8px;
            spacing: 8px;
            VerticalLayout {
                spacing: 6px;
                horizontal-stretch: 3;
                Text { text: "document (" + doc-path + ")"; font-size: 11px; }
                doc-edit := TextEdit {
                    text: "";
                    vertical-stretch: 1;
                    font-size: 13px;
                }
                HorizontalLayout {
                    spacing: 6px;
                    Button { text: "Open"; clicked => { send-open(); } }
                    Button { text: "Send change"; clicked => { send-change(); } }
                    Button { text: "Rescan"; clicked => { send-rescan(); } }
                    Button { text: "Pause"; clicked => { send-pause(); } }
                    Button { text: "Resume"; clicked => { send-resume(); } }
                }
            }
            VerticalLayout {
                spacing: 6px;
                horizontal-stretch: 2;
                Text { text: "engine notices"; font-size: 11px; }
                notice-log := TextEdit {
                    text: "";
                    read-only: true;
                    vertical-stretch: 2;
                    font-size: 11px;
                }
                Text { text: "out buffer (applied truncates/appends)"; font-size: 11px; }
                out-buffer-text := TextEdit {
                    text: "";
                    read-only: true;
                    vertical-stretch: 1;
                    font-size: 11px;
                }
            }
        }
    }
}

/// Plain-data event the reader thread sends to the UI thread.
struct UiEvent {
    raw: String,
    out_buffer: String,
}

/// Mirror the `out` info buffer exactly the way an editor plugin would:
/// truncates reset it, appends splice at `pos`. Returns the new buffer text.
fn apply_out_buffer(buffer: &mut String, notice: &WireNotice) {
    match notice {
        WireNotice::Truncate {
            buffer: InfoBuffer::Out,
            ..
        } => buffer.clear(),
        WireNotice::Append {
            buffer: InfoBuffer::Out,
            pos: Some(pos),
            text,
            ..
        } => {
            let start = (*pos).min(buffer.len());
            buffer.insert_str(start, text);
        }
        _ => {}
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
    let binary = binary.unwrap_or_else(|| {
        // Default: the oxipresso binary next to this one (same build tree).
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
    let protocol = if json {
        WireProtocol::Json
    } else {
        WireProtocol::Sexp
    };

    let session = match EditorWireSession::spawn(&binary, &doc_path, protocol, false) {
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

    // Reader: owns the session wire and the out-buffer mirror; the engine
    // session is moved here. It forwards queued UI commands (non-blocking)
    // and polls notices with a short timeout, so one thread serves both
    // directions.
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();
    let (wire_tx, wire_rx) = std::sync::mpsc::channel::<String>();
    {
        let ui_tx = ui_tx.clone();
        std::thread::spawn(move || {
            let mut session = session;
            let mut out_buffer = String::new();
            loop {
                while let Ok(line) = wire_rx.try_recv() {
                    if session.send_raw(&line).is_err() {
                        return;
                    }
                }
                if let Some(parsed) = session.next_notice(std::time::Duration::from_millis(100)) {
                    apply_out_buffer(&mut out_buffer, &parsed.notice);
                    let event = UiEvent {
                        raw: parsed.raw,
                        out_buffer: out_buffer.clone(),
                    };
                    if ui_tx.send(event).is_err() {
                        return;
                    }
                }
                // No notice in this window: loop back to the command queue.
            }
        });
    }

    // Buttons queue raw wire lines to the reader thread (Slint components
    // are not Send, so nothing UI-owned crosses a thread here). Each closure
    // gets its own strong component clone (named separately: the handler
    // registration borrows the original handle, so the closure cannot move
    // a shadowed binding of it); the doc path and protocol are captured by
    // value.
    {
        let ui_for_open = ui.clone_strong();
        let wire_tx = wire_tx.clone();
        let doc_path = doc_path.clone();
        ui.on_send_open(move || {
            let content = ui_for_open.get_document_text().to_string();
            use base64::Engine as _;
            let payload = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
            let line = format!(
                "(open-base64 {} \"{}\")",
                escape_wire_string(&doc_path.to_string_lossy()),
                payload
            );
            let _ = wire_tx.send(line);
        });
    }
    {
        let ui_for_change = ui.clone_strong();
        let wire_tx = wire_tx.clone();
        let doc_path = doc_path.clone();
        ui.on_send_change(move || {
            let new_text = ui_for_change.get_document_text().to_string();
            // Whole-buffer replace, matching what an editor save does: write
            // the file and replace bytes 0..old_len with the editor content.
            let old_len = std::fs::read(&doc_path)
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            let _ = std::fs::write(&doc_path, new_text.as_bytes());
            let line = if protocol == WireProtocol::Json {
                format!(
                    "[\"change\",{},{},{},{}]",
                    json_string(&doc_path.to_string_lossy()),
                    0,
                    old_len,
                    json_string(&new_text)
                )
            } else {
                format!(
                    "(change {} 0 {old_len} {})",
                    escape_wire_string(&doc_path.to_string_lossy()),
                    escape_wire_string(&new_text)
                )
            };
            let _ = wire_tx.send(line);
        });
    }
    ui.on_send_rescan({
        let wire_tx = wire_tx.clone();
        move || {
            let _ = wire_tx.send("(rescan)".to_string());
        }
    });
    ui.on_send_pause({
        let wire_tx = wire_tx.clone();
        move || {
            let _ = wire_tx.send("(pause)".to_string());
        }
    });
    ui.on_send_resume({
        let wire_tx = wire_tx.clone();
        move || {
            let _ = wire_tx.send("(resume)".to_string());
        }
    });

    // UI-side timer: drain reader events onto the properties.
    let timer = slint::Timer::default();
    {
        let ui_weak = ui.as_weak();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                while let Ok(event) = ui_rx.try_recv() {
                    let mut log = ui.get_message_log().to_string();
                    if log.len() > 128 * 1024 {
                        log = log[log.len() - 96 * 1024..].to_string();
                    }
                    log.push_str(&event.raw);
                    log.push('\n');
                    ui.set_message_log(log.into());
                    ui.set_out_buffer(SharedString::from(event.out_buffer));
                }
            },
        );
    }

    ui.run().expect("slint event loop");
}
