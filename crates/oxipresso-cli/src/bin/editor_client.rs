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
    import { TextEdit } from "std-widgets.slint";

    // 工具栏按钮：圆角、悬停变色、按下加深
    component ToolButton inherits Rectangle {
        in property <string> label;
        in property <bool> enabled: true;
        in property <length> min-w: 34px;
        callback clicked();
        height: 30px;
        min-width: root.min-w;
        border-radius: 6px;
        ta := TouchArea {
            enabled: root.enabled;
            mouse-cursor: root.enabled ? MouseCursor.pointer : MouseCursor.default;
            clicked => { root.clicked(); }
        }
        background: ta.pressed ? #45475a : (ta.has-hover ? #3c3c54 : transparent);
        animate background { duration: 110ms; }
        Text {
            text: root.label;
            color: root.enabled ? #cdd6f4 : #585b70;
            font-size: 13px;
            horizontal-alignment: center;
            vertical-alignment: center;
        }
    }

    export component EditorClientWindow inherits Window {
        title: "Oxipresso — live TeX preview";
        background: #1e1e2e;
        preferred-width: 1440px;
        preferred-height: 920px;

        in property <string> doc-path;
        in-out property <string> document-text <=> doc-edit.text;
        in-out property <image> page-image;
        in-out property <string> engine-log <=> log-edit.text;
        in-out property <string> status;
        in property <int> page-index;
        in property <int> page-count;
        in-out property <float> zoom: 1.0;
        in property <bool> editor-open: true;
        in-out property <length> preview-width <=> flick.width;
        in property <bool> log-open: false;
        in property <float> page-ratio: 1.414;
        callback editor-edited();
        callback prev-page();
        callback next-page();
        callback toggle-editor();
        callback toggle-log();
        callback zoom-in();
        callback zoom-out();
        callback push-now();

        VerticalLayout {
            // ── 工具栏 ────────────────────────────────────────────
            Rectangle {
                height: 46px;
                background: #181825;
                HorizontalLayout {
                    padding-left: 14px;
                    padding-right: 14px;
                    spacing: 8px;
                    alignment: center;
                    Text { text: "◆"; color: #89b4fa; font-size: 15px; vertical-alignment: center; }
                    Text { text: "Oxipresso"; color: #cdd6f4; font-size: 14px; font-weight: 700; vertical-alignment: center; }
                    Rectangle { width: 1px; height: 22px; background: #313244; }
                    ToolButton {
                        label: editor-open ? "◁ 源码" : "▷ 源码";
                        clicked => { toggle-editor(); }
                    }
                    ToolButton { label: "◀"; min-w: 30px; clicked => { prev-page(); } }
                    Rectangle {
                        min-width: 74px;
                        height: 30px;
                        border-radius: 6px;
                        background: #11111b;
                        Text {
                            text: (page-count > 0 ? page-index + 1 : 0) + " / " + page-count;
                            color: #a6adc8;
                            font-size: 12px;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                    }
                    ToolButton { label: "▶"; min-w: 30px; clicked => { next-page(); } }
                    Rectangle { width: 8px; }
                    ToolButton { label: "－"; min-w: 30px; clicked => { zoom-out(); } }
                    Rectangle {
                        min-width: 56px;
                        height: 30px;
                        border-radius: 6px;
                        background: #11111b;
                        Text {
                            text: round(zoom * 100) + "%";
                            color: #a6adc8;
                            font-size: 12px;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                    }
                    ToolButton { label: "＋"; min-w: 30px; clicked => { zoom-in(); } }
                    Rectangle { horizontal-stretch: 1; }
                    ToolButton {
                        label: log-open ? "▽ 日志" : "△ 日志";
                        clicked => { toggle-log(); }
                    }
                    push-btn := ToolButton {
                        label: "推送 ⇧";
                        min-w: 72px;
                        clicked => { push-now(); }
                    }
                }
            }
            Rectangle { height: 1px; background: #313244; }
            // ── 主区域 ────────────────────────────────────────────
            HorizontalLayout {
                spacing: 0px;
                // 编辑面板（宽度动画折叠）
                Rectangle {
                    width: editor-open ? 400px : 0px;
                    animate width { duration: 220ms; easing: ease-out; }
                    background: #181825;
                    clip: true;
                    VerticalLayout {
                        padding: 8px;
                        spacing: 4px;
                        Text { text: "TeX 源码 · 修改即推送"; color: #6c7086; font-size: 11px; }
                        doc-edit := TextEdit {
                            text: "";
                            vertical-stretch: 1;
                            font-size: 13px;
                            edited => { editor-edited(); }
                        }
                    }
                }
                Rectangle { width: 1px; background: #313244; }
                // 预览：Flickable 支持缩放平移
                Rectangle {
                    background: #11111b;
                    horizontal-stretch: 1;
                    clip: true;
                    flick := Flickable {
                        Rectangle {
                            width: flick.width * zoom;
                            height: flick.width * zoom * page-ratio;
                            preview-holder := Rectangle {
                                background: #ffffff;
                                width: parent.width;
                                height: parent.height;
                                page-view := Image {
                                    x: 0; y: 0;
                                    width: parent.width;
                                    height: parent.height;
                                    source: page-image;
                                }
                            }
                        }
                    }
                    // 键盘翻页
                    FocusScope {
                        width: parent.width;
                        height: parent.height;
                        key-pressed(e) => {
                            if e.text == Key.RightArrow { next-page(); return accept; }
                            if e.text == Key.LeftArrow { prev-page(); return accept; }
                            reject
                        }
                    }
                }
            }
            // The preview pane's width (logical px): the render worker
            // matches the page bitmap to the on-screen physical pixels so
            // the preview is crisp instead of upscaled-blurry.
            // ── 日志面板（折叠） ──────────────────────────────────
            Rectangle {
                height: log-open ? 150px : 0px;
                animate height { duration: 220ms; easing: ease-out; }
                background: #181825;
                clip: true;
                VerticalLayout {
                    padding: 6px;
                    log-edit := TextEdit {
                        text: "";
                        read-only: true;
                        font-size: 11px;
                    }
                }
            }
            Rectangle { height: 1px; background: #313244; }
            // ── 状态栏 ────────────────────────────────────────────
            Rectangle {
                height: 26px;
                background: #181825;
                HorizontalLayout {
                    padding-left: 14px;
                    padding-right: 14px;
                    spacing: 12px;
                    alignment: center;
                    Text { text: doc-path; color: #585b70; font-size: 11px; vertical-alignment: center; overflow: elide; }
                    Rectangle { horizontal-stretch: 1; }
                    Text { text: status; color: #a6e3a1; font-size: 11px; vertical-alignment: center; }
                }
            }
        }
    }
}

/// Plain-data events from the workers to the UI timer (all Send).
enum UiEvent {
    Page(SharedPixelBuffer, usize, String),
    Log(String),
    Pages(usize),
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
    // The env fallback for the auto-edit (convenient for detached launches).
    let auto_edit = auto_edit.or_else(|| std::env::var("OXI_CLIENT_AUTOEDIT").ok());
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
            // In-flight throttle: the CLI runs ONE hot pass per change and
            // processes queued changes serially, so a typing burst would cost
            // one full pass per keystroke. Hold the latest text while a pass
            // is running (the engine absorbs it into the next one) and send
            // only after the pass's `(flush)` notice — a burst costs a single
            // pass. The timeout covers changes that trigger no pass at all
            // (the read_files rebuild-skip emits no flush).
            let mut in_flight = false;
            let mut last_send = std::time::Instant::now();

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
                let may_send =
                    !in_flight || last_send.elapsed() >= std::time::Duration::from_millis(1200);
                if may_send && let Some(text) = pending_text.take() {
                    in_flight = true;
                    last_send = std::time::Instant::now();
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
                    // The pass-end marker releases the in-flight throttle.
                    if matches!(parsed.notice, WireNotice::Flush) {
                        in_flight = false;
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
    // (len, mtime) — an equal-length rebuild still updates — page-request
    // handling for the toolbar navigation, and pixel buffers to the UI.
    let (page_req_tx, page_req_rx) = std::sync::mpsc::channel::<usize>();
    // Target bitmap width in physical pixels (f32 bits): pane width × zoom ×
    // the window's scale factor. Matching the bitmap to the display makes the
    // preview pixel-crisp instead of upscaled-blurry at DPI scaling / zoom.
    let render_density: std::sync::Arc<std::sync::atomic::AtomicU32> =
        std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let artifact_path = artifact_path.clone();
        let ui_tx = ui_tx.clone();
        let render_density = render_density.clone();
        std::thread::spawn(move || {
            let backend = XdvGlyphRenderBackend::new(Box::new(
                KpseFontResolver::detect().unwrap_or_else(|| KpseFontResolver::dummy()),
            ));
            let mut fingerprint: Option<(usize, u64)> = None;
            let mut artifact_bytes: Vec<u8> = Vec::new();
            let mut artifact_version: u64 = 0;
            let mut requested_page: usize = 0;
            let mut rendered: Option<(u64, usize)> = None;
            let mut extra_scale: f64 = 1.0;
            loop {
                // Artifact change detection (len + mtime).
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
                if current.is_some() && current != fingerprint {
                    fingerprint = current;
                    if let Ok(bytes) = std::fs::read(&artifact_path) {
                        artifact_bytes = bytes;
                        artifact_version += 1;
                        // Page count for the toolbar (parse is cheap: a few
                        // hundred KB, done once per new artifact).
                        if let Ok(doc) =
                            oxipresso_render::xdv::parse_xdv(&artifact_bytes, &mut |_| None)
                        {
                            let _ = ui_tx.send(UiEvent::Pages(doc.pages.len()));
                        }
                        rendered = None; // force a re-render of the page
                    }
                }
                // Display-matched density: the UI publishes the on-screen
                // target width; re-render when it moves by more than 2%.
                let target_w =
                    f32::from_bits(render_density.load(std::sync::atomic::Ordering::Relaxed));
                if target_w > 0.0
                    && let Some((w_pt, _)) = backend.page_size_pt(
                        &DocumentArtifact {
                            kind: ArtifactKind::Xdv,
                            bytes: artifact_bytes.clone(),
                            source_name: None,
                        },
                        requested_page,
                    )
                {
                    let ideal: f64 = (target_w as f64) / (w_pt * backend.px_per_pt);
                    let ideal = ideal.clamp(0.5, 4.0);
                    if (ideal - extra_scale).abs() / extra_scale.max(0.001) > 0.02 {
                        extra_scale = ideal;
                        rendered = None;
                    }
                }
                // Page requests from the toolbar: keep the latest.
                let mut req = None;
                while let Ok(p) = page_req_rx.try_recv() {
                    req = Some(p);
                }
                if let Some(p) = req {
                    if p != requested_page {
                        requested_page = p;
                        rendered = None;
                    }
                }
                // Render the requested page of the current artifact version.
                if rendered != Some((artifact_version, requested_page))
                    && !artifact_bytes.is_empty()
                {
                    let t_render = std::time::Instant::now();
                    let artifact = DocumentArtifact {
                        kind: ArtifactKind::Xdv,
                        bytes: artifact_bytes.clone(),
                        source_name: Some(artifact_path.to_string_lossy().into_owned()),
                    };
                    match backend.render_page_scaled(&artifact, requested_page, extra_scale as f32)
                    {
                        Ok(page) => {
                            let ratio = if page.width > 0 {
                                page.height as f32 / page.width as f32
                            } else {
                                1.414
                            };
                            let buffer = SharedPixelBuffer {
                                rgba: page.pixels_rgba,
                                width: page.width,
                                height: page.height,
                            };
                            let size = artifact.bytes.len();
                            let ms = (std::time::Instant::now() - t_render).as_millis();
                            let _ = ui_tx.send(UiEvent::Page(
                                buffer,
                                requested_page,
                                format!(
                                    "第 {} 页 · {size} B XDV · 渲染 {ms}ms",
                                    requested_page + 1
                                ),
                            ));
                        }
                        Err(error) => {
                            // Out-of-range page: the artifact is a mid-pass
                            // prefix (streaming preview) or genuinely
                            // shrank. Keep the current texture and the
                            // requested page — yanking to page 0 would
                            // throw the reader out of their position every
                            // pass. The next artifact version retries.
                            let _ = error;
                            rendered = Some((artifact_version, requested_page));
                        }
                    }
                    rendered = Some((artifact_version, requested_page));
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        });
    }

    // UI thread: adopt worker events into the panes.
    let timer = slint::Timer::default();
    {
        let ui_handle = ui.clone_strong();
        let render_density = render_density.clone();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(30),
            move || {
                let mut log_tail: Option<String> = None;
                let mut page_event: Option<(SharedPixelBuffer, usize, String)> = None;
                let mut pages: Option<usize> = None;
                while let Ok(event) = ui_rx.try_recv() {
                    match event {
                        UiEvent::Log(tail) => log_tail = Some(tail),
                        UiEvent::Pages(count) => pages = Some(count),
                        UiEvent::Page(buffer, page, status) => {
                            page_event = Some((buffer, page, status))
                        }
                    }
                }
                // Publish the on-screen target width (logical pane × zoom ×
                // DPI scale) so the render worker draws at physical-pixel
                // density instead of upscaling a 96-dpi bitmap.
                let target = ui_handle.get_preview_width()
                    * ui_handle.get_zoom()
                    * ui_handle.window().scale_factor();
                render_density.store(
                    (target.max(0.0) as f32).to_bits(),
                    std::sync::atomic::Ordering::Relaxed,
                );
                if let Some(tail) = log_tail {
                    ui_handle.set_engine_log(SharedString::from(tail.as_str()));
                }
                if let Some(count) = pages {
                    ui_handle.set_page_count(count as i32);
                }
                if let Some((buffer, page, status)) = page_event {
                    if buffer.width > 0 {
                        let image =
                            slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(
                                &buffer.rgba,
                                buffer.width,
                                buffer.height,
                            ));
                        ui_handle.set_page_image(image);
                        ui_handle.set_page_ratio(if buffer.width > 0 {
                            buffer.height as f32 / buffer.width as f32
                        } else {
                            1.414
                        });
                    }
                    ui_handle.set_page_index(page as i32);
                    ui_handle.set_status(SharedString::from(status));
                }
            },
        );
    }

    // Editor edits and Push: hand the CURRENT buffer text to the wire
    // worker (it coalesces and performs the possibly-blocking send).
    {
        let edit_text_tx_editor = edit_text_tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_editor_edited(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = edit_text_tx_editor.send(ui.get_document_text().to_string());
            }
        });
        let edit_text_tx_push = edit_text_tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_push_now(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = edit_text_tx_push.send(ui.get_document_text().to_string());
            }
        });
    }
    // Page navigation + zoom + panel toggles.
    {
        let ui_weak_prev = ui.as_weak();
        let page_req_prev = page_req_tx.clone();
        ui.on_prev_page(move || {
            if let Some(ui) = ui_weak_prev.upgrade() {
                let p = (ui.get_page_index() - 1).max(0);
                ui.set_page_index(p);
                let _ = page_req_prev.send(p as usize);
            }
        });
        let ui_weak_next = ui.as_weak();
        let page_req_next = page_req_tx.clone();
        ui.on_next_page(move || {
            if let Some(ui) = ui_weak_next.upgrade() {
                let p = (ui.get_page_index() + 1).min(ui.get_page_count().saturating_sub(1));
                ui.set_page_index(p);
                let _ = page_req_next.send(p as usize);
            }
        });
        let ui_weak = ui.as_weak();
        ui.on_toggle_editor(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_editor_open(!ui.get_editor_open());
            }
        });
        let ui_weak = ui.as_weak();
        ui.on_toggle_log(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_log_open(!ui.get_log_open());
            }
        });
        let ui_weak = ui.as_weak();
        ui.on_zoom_in(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_zoom((ui.get_zoom() * 1.25).min(4.0));
            }
        });
        let ui_weak = ui.as_weak();
        ui.on_zoom_out(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_zoom((ui.get_zoom() / 1.25).max(0.4));
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
                    let edited = current.replace(
                        "分类旨在从头皮",
                        &format!("分类旨在（{{\\bfseries {}}}）从头皮", auto_text),
                    );
                    let _ = edit_text_tx.send(edited);
                    ui.set_status(SharedString::from("auto-edit pushed"));
                }
            },
        );
        std::mem::forget(auto_timer);
    }

    ui.run().expect("slint event loop");
}
