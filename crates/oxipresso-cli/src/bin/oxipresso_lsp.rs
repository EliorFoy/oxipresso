//! `oxipresso-lsp` — a Language Server Protocol bridge in front of the
//! Oxipresso render server. Editors that speak LSP (Zed, VS Code, ...) send
//! `textDocument/didChange` here; this bridge drives the resident engine's
//! hot pass and answers with `publishDiagnostics` parsed from the TeX log,
//! while writing the artifact / SyncTeX sidecar / edit-line file that the
//! companion preview follows.

use oxipresso_editor_protocol::wire::EditorWireSession;
use oxipresso_editor_protocol::{InfoBuffer, WireNotice, WireProtocol};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const MAX_DIAGNOSTICS: usize = 50;

fn main() -> std::process::ExitCode {
    let mut doc_path: Option<PathBuf> = None;
    let mut binary: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--binary" => binary = args.next().map(PathBuf::from),
            other => {
                if doc_path.is_none() {
                    doc_path = Some(PathBuf::from(other));
                }
            }
        }
    }
    let Some(doc_path) = doc_path else {
        eprintln!("usage: oxipresso-lsp [--binary PATH] <document.tex>");
        return std::process::ExitCode::from(2);
    };

    match run(doc_path, binary) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("[oxipresso-lsp] {error}");
            std::process::ExitCode::from(1)
        }
    }
}

struct Bridge {
    session: EditorWireSession,
    uri: String,
    wire_name: String,
    line_file: PathBuf,
    last_sent: String,
    log_text: String,
}

fn run(doc_path: PathBuf, binary: Option<PathBuf>) -> Result<(), String> {
    let wire_name = doc_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| "document path has no file name".to_string())?;
    let uri = format!("file://{}", doc_path.to_string_lossy());
    let line_file = doc_path.with_extension("oxiline");

    // The companion preview watches these three files.
    // (edition 2024: env mutation is unsafe; this process owns its env.)
    unsafe {
        std::env::set_var("OXIPRESSO_RESIDENT", "1");
        std::env::set_var("OXIPRESSO_ARTIFACT_OUT", doc_path.with_extension("xdv"));
        std::env::set_var("OXIPRESSO_SYNCTEX_OUT", doc_path.with_extension("synctex"));
    }

    let engine = binary.unwrap_or_else(|| {
        let exe = std::env::current_exe().expect("current exe");
        exe.with_file_name(if cfg!(windows) { "oxipresso.exe" } else { "oxipresso" })
    });
    eprintln!(
        "[oxipresso-lsp] doc={} engine={}",
        doc_path.display(),
        engine.display()
    );

    let mut bridge: Option<Bridge> = None;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    while let Some(message) = read_message(&mut reader) {
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        match method.as_str() {
            "initialize" => {
                let reply = json!({
                    "id": id,
                    "result": {
                        "capabilities": {
                            "textDocumentSync": 1,
                            "serverInfo": {"name": "oxipresso-lsp"}
                        }
                    }
                });
                send_msg(&mut out, &reply).map_err(|e| e.to_string())?;
            }
            "shutdown" => {
                let reply = json!({"id": id, "result": null});
                send_msg(&mut out, &reply).map_err(|e| e.to_string())?;
            }
            "exit" => {
                if let Some(bridge) = bridge.as_mut() {
                    bridge.session.shutdown();
                }
                return Ok(());
            }
            "textDocument/didOpen" => {
                let text = message
                    .pointer("/params/textDocument/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let session =
                    EditorWireSession::spawn(&engine, &doc_path, WireProtocol::Sexp, true)
                        .map_err(|e| format!("engine spawn failed: {e}"))?;
                bridge = Some(Bridge {
                    session,
                    uri: uri.clone(),
                    wire_name: wire_name.clone(),
                    line_file: line_file.clone(),
                    last_sent: String::new(),
                    log_text: String::new(),
                });
                let bridge = bridge.as_mut().expect("just set");
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                let _ = bridge
                    .session
                    .send_raw(&format!("(register {})", escape(&bridge.wire_name)));
                let _ = bridge.session.send_raw(&format!(
                    "(open-base64 {} \"{}\")",
                    escape(&bridge.wire_name),
                    b64
                ));
                let _ = bridge.session.send_raw("(resume)");
                let _ = bridge.session.send_raw(&format!(
                    "(change {} 0 {} {})",
                    escape(&bridge.wire_name),
                    text.len(),
                    escape(&text)
                ));
                bridge.last_sent = text;
                let _ = bridge.session.send_raw("(rescan)");
                settle_pass(bridge, &mut out)?;
            }
            "textDocument/didChange" => {
                let Some(bridge) = bridge.as_mut() else { continue };
                let text = message
                    .pointer("/params/contentChanges")
                    .and_then(|c| c.as_array())
                    .and_then(|changes| changes.last())
                    .and_then(|change| change.pointer("/text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                if text == bridge.last_sent || text.is_empty() {
                    continue;
                }
                let edit_offset = first_diff(&bridge.last_sent, &text) as usize;
                let edit_line = 1 + snap_boundary(&text, edit_offset).matches('\n').count();
                let old_len = bridge.last_sent.len();
                bridge.last_sent = text.clone();
                let line = format!(
                    "(change {} 0 {old_len} {})",
                    escape(&bridge.wire_name),
                    escape(&text)
                );
                if bridge.session.send_raw(&line).is_err() {
                    return Err("engine died".to_string());
                }
                settle_pass(bridge, &mut out)?;
                let _ = std::fs::write(&bridge.line_file, edit_line.to_string());
            }
            "textDocument/definition" => {
                // Reverse SyncTeX lands here in a later iteration.
                let reply = json!({"id": id, "result": null});
                send_msg(&mut out, &reply).map_err(|e| e.to_string())?;
            }
            _ => {
                if id.is_some() {
                    let reply = json!({
                        "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    });
                    send_msg(&mut out, &reply).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(())
}

/// Drains the engine's notices until the pass settles (a flush after the
/// log/out streams), collecting the TeX log for diagnostics.
fn settle_pass(bridge: &mut Bridge, out: &mut dyn Write) -> Result<(), String> {
    bridge.log_text.clear();
    // Halt-on-error docs abort the pass mid-way: the wire goes quiet and the
    // CLI self-heals with a full restart. Detect that, publish the partial
    // log's errors, and keep serving - never wedge or exit on a broken doc.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::time::Instant::now() > deadline {
            break;
        }
        if bridge.session.stderr_text().contains("falling back to a full restart") {
            break;
        }
        let Some(parsed) = bridge.session.next_notice(Duration::from_millis(200)) else {
            continue;
        };
        match &parsed.notice {
            WireNotice::Truncate {
                buffer: InfoBuffer::Log,
                ..
            } => bridge.log_text.clear(),
            WireNotice::Append {
                buffer: InfoBuffer::Log,
                text,
                ..
            } => bridge.log_text.push_str(text),
            WireNotice::Flush => break,
            _ => {}
        }
    }
    publish_diagnostics(bridge, out);
    Ok(())
}

/// TeX log "! ..." blocks -> LSP diagnostics (severity 1, 0-based line from
/// the following `l.<n>` marker).
fn parse_log_errors(log: &str) -> Vec<(u32, String)> {
    let lines: Vec<&str> = log.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() && out.len() < MAX_DIAGNOSTICS {
        let Some(message) = lines[i].strip_prefix("! ") else {
            i += 1;
            continue;
        };
        let mut line_no = 0u32;
        let mut j = i + 1;
        while j < lines.len() && j < i + 14 {
            if let Some(rest) = lines[j].strip_prefix("l.") {
                line_no = rest
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or(0);
                break;
            }
            if lines[j].starts_with('!') {
                break;
            }
            j += 1;
        }
        out.push((line_no.saturating_sub(1), message.to_string()));
        i = j.max(i + 1);
    }
    out
}

fn publish_diagnostics(bridge: &Bridge, out: &mut dyn Write) {
    let diagnostics: Vec<Value> = parse_log_errors(&bridge.log_text)
        .into_iter()
        .map(|(line, message)| {
            json!({
                "range": {
                    "start": {"line": line, "character": 0},
                    "end": {"line": line, "character": 0}
                },
                "severity": 1,
                "source": "oxipresso",
                "message": message
            })
        })
        .collect();
    let message = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {"uri": bridge.uri, "diagnostics": diagnostics}
    });
    let _ = send_msg(out, &message);
}

fn snap_boundary(text: &str, offset: usize) -> &str {
    let mut boundary = offset.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &text[..boundary]
}

fn first_diff(previous: &str, current: &str) -> u64 {
    let previous = previous.as_bytes();
    let current = current.as_bytes();
    let common = previous.len().min(current.len());
    let mut index = 0usize;
    while index < common && previous[index] == current[index] {
        index += 1;
    }
    index as u64
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length: ") {
            content_length = value.trim().parse().ok();
        }
    }
    let length = content_length?;
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer).ok()?;
    serde_json::from_slice(&buffer).ok()
}

fn send_msg(out: &mut dyn Write, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    out.flush()
}