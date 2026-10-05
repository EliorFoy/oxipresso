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

use oxipresso_cli::editor_wire::{EditorWireSession, WireNotice, escape_wire_string};
use oxipresso_editor_protocol::{InfoBuffer, WireProtocol};
use slint::{ComponentHandle, Weak};

slint::slint! {
    import { Button, TextEdit } from "std-widgets.slib";

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

struct EditorWindow {
    ui: EditorClientWindow,
    session: std::sync::Mutex<Option<EditorWireSession>>,
    doc_path: std::path::PathBuf,
    protocol: WireProtocol,
}

impl EditorWindow {
    fn log_line(&self, line: &str) {
        let ui = self.ui.as_weak();
        let line = line.to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                let mut log = ui.get_message_log();
                // Cap the log so a long session cannot grow without bound.
                if log.len() > 128 * 1024 {
                    log = log[log.len() - 96 * 1024..].to_string();
                }
                log.push_str(&line);
                log.push('\n');
                ui.set_message_log(log);
            }
        });
    }

    fn send_raw(&self, line: String) {
        let mut guard = self.session.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(session) = guard.as_mut() {
            if let Err(error) = session.send_raw(&line) {
                self.log_line(&format!("[client] {error}"));
            }
        } else {
            self.log_line("[client] no engine session");
        }
    }
}

/// Mirror the `out` info buffer exactly the way an editor plugin would:
/// truncates reset it, appends splice at `pos`, and the pane shows the text.
fn apply_out_buffer(ui: &Weak<EditorClientWindow>, notice: &WireNotice) {
    if let Some(ui) = ui.upgrade() {
        match notice {
            WireNotice::Truncate {
                buffer: InfoBuffer::Out,
                ..
            } => ui.set_out_buffer(String::new()),
            WireNotice::Append {
                buffer: InfoBuffer::Out,
                pos: Some(pos),
                text,
                ..
            } => {
                let mut buf = ui.get_out_buffer();
                let start = (*pos).min(buf.len());
                buf.insert_str(start, text);
                ui.set_out_buffer(buf);
            }
            _ => {}
        }
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

    let window = std::sync::Arc::new(EditorWindow {
        ui: ui.clone_strong(),
        session: std::sync::Mutex::new(Some(session)),
        doc_path: doc_path.clone(),
        protocol,
    });

    // Reader: engine notices -> log pane + out-buffer mirror.
    {
        let window = std::sync::Arc::clone(&window);
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            loop {
                let parsed = {
                    let mut guard = window.session.lock().unwrap_or_else(|p| p.into_inner());
                    match guard.as_mut() {
                        Some(session) => session.next_notice(std::time::Duration::from_secs(1)),
                        None => return,
                    }
                };
                let Some(parsed) = parsed else { continue };
                window.log_line(&parsed.raw);
                let _ = slint::invoke_from_event_loop(move || {
                    apply_out_buffer(&ui_weak, &parsed.notice);
                });
            }
        });
    }

    // Buttons. The on_* handles must outlive the event loop (dropping one
    // disconnects its handler), so bind each to a named local.
    {
        let window = std::sync::Arc::clone(&window);
        ui.on_send_open(move || {
            let content = window.ui.get_document_text().to_string();
            use base64::Engine as _;
            let payload = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
            let line = if window.protocol == WireProtocol::Json {
                format!(
                    "[\"open-base64\",{},\"{payload}\"]",
                    json_string(&window.doc_path.to_string_lossy())
                )
            } else {
                format!(
                    "(open-base64 {} \"{}\")",
                    escape_wire_string(&window.doc_path.to_string_lossy()),
                    payload
                )
            };
            window.send_raw(line);
        });
    }
    {
        let window = std::sync::Arc::clone(&window);
        ui.on_send_change(move || {
            let new_text = window.ui.get_document_text().to_string();
            let old_len = std::fs::read(&window.doc_path)
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            let _ = std::fs::write(&window.doc_path, new_text.as_bytes());
            let line = if window.protocol == WireProtocol::Json {
                format!(
                    "[\"change\",{},{},{},{}]",
                    json_string(&window.doc_path.to_string_lossy()),
                    0,
                    old_len,
                    json_string(&new_text)
                )
            } else {
                format!(
                    "(change {} 0 {old_len} {})",
                    escape_wire_string(&window.doc_path.to_string_lossy()),
                    escape_wire_string(&new_text)
                )
            };
            window.send_raw(line);
        });
    }
    let _rescan_handle = {
        let window = std::sync::Arc::clone(&window);
        ui.on_send_rescan(move || window.send_raw("(rescan)".to_string()))
    };
    let _pause_handle = {
        let window = std::sync::Arc::clone(&window);
        ui.on_send_pause(move || window.send_raw("(pause)".to_string()))
    };
    let _resume_handle = {
        let window = std::sync::Arc::clone(&window);
        ui.on_send_resume(move || window.send_raw("(resume)".to_string()))
    };

    ui.run().expect("slint event loop");
}
