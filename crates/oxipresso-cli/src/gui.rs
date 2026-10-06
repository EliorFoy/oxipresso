//! Live preview GUI: the engine, the egui viewer, and the editor wire
//! (stdin/stdout) run in one process — the TeXpresso architecture. The
//! editor sends protocol commands over stdin; engine artifacts refresh the
//! window directly; reverse SyncTeX clicks produce editor notifications.

use std::io::{BufRead, Write};
use std::sync::mpsc;

use eframe::egui;
use oxipresso_editor_protocol::{EditorMessage, serialize_message};
use oxipresso_platform::{FileWatchEvent, FileWatcher};
#[cfg(feature = "freetype")]
use oxipresso_render::XdvGlyphRenderBackend;
use oxipresso_render::{AutoRenderBackend, RenderBackend};
use oxipresso_viewer::ViewerState;

use crate::{CliOptions, OxipressoApp, disk_roots_for, root_document};
#[cfg(feature = "freetype")]
use crate::{DocumentImageLoader, KpseFontResolver};

/// Runs the live preview window; blocks until the window closes.
pub fn run_live_preview(options: CliOptions) -> Result<(), String> {
    let root = root_document(&options)?;
    let doc_path = root.root_dir.join(&root.root_name);
    let renderer = {
        // Real XDV rendering in the preview: glyphs + rules + images through
        // the glyph backend (placeholder page without the freetype feature).
        let renderer = AutoRenderBackend::default();
        #[cfg(feature = "freetype")]
        let renderer = renderer.with_xdv_glyph_backend(XdvGlyphRenderBackend::with_image_loader(
            Box::new(KpseFontResolver::detect().unwrap_or_else(|| KpseFontResolver::dummy())),
            Box::new(DocumentImageLoader {
                roots: disk_roots_for(&root),
            }),
        ));
        renderer
    };
    let mut app = OxipressoApp::new(options, root);
    // Initialize the engine before the window appears so editor-facing
    // messages (lookup/input/stream) reach stdout in order.
    eprintln!(
        "[gui] initializing (resident={})",
        std::env::var("OXIPRESSO_RESIDENT").unwrap_or_default()
    );
    let pending = app.initialize().map_err(|error| error.to_string())?;
    eprintln!("[gui] initialize done");

    // Editor wire: stdin lines are pumped from a thread into the GUI loop.
    let (editor_tx, editor_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if editor_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    eprintln!("[gui] entering eframe");
    let options = eframe::NativeOptions::default();
    let mut host = LivePreview {
        app,
        editor_rx,
        pending,
        viewer: ViewerState::default(),
        renderer,
        texture: None,
        texture_key: None,
        rendered_size: None,
        rendered_scale: 1.0,
        centered_zoom: 1.0,
        status: String::new(),
        doc_watcher: oxipresso_platform::file_watcher(doc_path),
    };
    // The initial pass's artifact must reach the viewer BEFORE the first
    // frame: poll_editor_wire only refreshes after an editor command, so
    // without this the window shows "No document yet" forever.
    host.refresh_from_engine();
    eframe::run_native(
        "Oxipresso Live Preview",
        options,
        Box::new(move |_cc| Ok(Box::new(host))),
    )
    .map_err(|error| error.to_string())
}

struct LivePreview {
    app: OxipressoApp,
    editor_rx: mpsc::Receiver<String>,
    pending: Vec<EditorMessage>,
    viewer: ViewerState,
    renderer: AutoRenderBackend,
    texture: Option<egui::TextureHandle>,
    texture_key: Option<u64>,
    rendered_size: Option<egui::Vec2>,
    rendered_scale: f32,
    /// The zoom level the scroll view was last centered for.
    centered_zoom: f32,
    status: String,
    /// Watches the root document on disk: an external editor save triggers
    /// an automatic rebuild (the standalone-preview core loop).
    doc_watcher: Box<dyn FileWatcher>,
}

impl LivePreview {
    /// Drains queued editor lines through the protocol app and writes every
    /// produced editor message back over stdout.
    fn poll_editor_wire(&mut self) {
        let mut handled = false;
        while let Ok(line) = self.editor_rx.try_recv() {
            if line.trim().is_empty() {
                continue;
            }
            handled = true;
            match self.app.handle_editor_line(&line) {
                Ok(messages) => self.emit(&messages),
                Err(error) => self.status = error,
            }
        }
        if !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.emit(&pending);
        }
        if handled {
            self.refresh_from_engine();
        }
    }

    /// Polls the document watcher: an external save of the root file
    /// triggers a rebuild — a resident hot pass when a session is live,
    /// a full restart otherwise. This is the standalone-preview core loop.
    fn poll_document_watcher(&mut self) {
        let watcher = self.doc_watcher.as_mut();
        match watcher.poll() {
            FileWatchEvent::Changed { .. } => {
                watcher.mark_clean(None);
                if self.app.paused {
                    return;
                }
                match self.app.handle_editor_line("(rescan)") {
                    Ok(messages) => {
                        self.emit(&messages);
                        self.refresh_from_engine();
                    }
                    Err(error) => self.status = error,
                }
            }
            FileWatchEvent::Missing { .. } => {
                // The document is momentarily absent (atomic-save rename);
                // the next poll after the save completes will see it again.
            }
            FileWatchEvent::Unchanged => {}
        }
    }

    fn emit(&mut self, messages: &[EditorMessage]) {
        let protocol = self.app.options.protocol;
        let mut stdout = std::io::stdout().lock();
        for message in messages {
            let _ = writeln!(stdout, "{}", serialize_message(message, protocol));
        }
        let _ = stdout.flush();
    }

    /// Pulls the freshest artifact (the resident session's last pass when a
    /// session is live, otherwise the engine's own output) into the viewer
    /// state and refreshes the displayed page texture.
    fn refresh_from_engine(&mut self) {
        let Some(artifact) = self.app.current_artifact() else {
            return;
        };
        let changed = self
            .viewer
            .last_artifact
            .as_ref()
            .is_none_or(|previous| previous.bytes != artifact.bytes);
        if !changed {
            return;
        }
        if self
            .viewer
            .load_artifact_with_renderer(artifact, &self.renderer)
            .is_ok()
        {
            self.texture = None;
            self.texture_key = None;
            self.rendered_size = None;
        }
    }

    fn page_image(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        self.ensure_texture(ui.ctx(), available);
        let Some(texture) = self.texture.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label("No document yet");
            });
            return;
        };
        let rendered_size = self.rendered_size.unwrap_or_else(|| texture.size_vec2());
        let available = ui.available_size();
        let fit_scale = match self.viewer.fit_mode {
            oxipresso_viewer::FitMode::Page => (available.x / rendered_size.x)
                .min(available.y / rendered_size.y)
                .min(1.0),
            oxipresso_viewer::FitMode::Width => (available.x / rendered_size.x).min(4.0),
        };
        let display_size = rendered_size * fit_scale * self.viewer.zoom;
        // Zoom keeps the page CENTERED in view (TeXpresso behavior): without
        // this the scroll offset stays at the top-left corner, which after a
        // zoom-in shows the blank page margin — the zoom appears to "do
        // nothing". Re-center on every zoom change.
        let zoom_changed = self.centered_zoom != self.viewer.zoom;
        let mut scroll_area = egui::ScrollArea::both().auto_shrink([false, false]);
        if zoom_changed {
            let oversize = display_size - available;
            if oversize.x > 0.0 {
                scroll_area = scroll_area.horizontal_scroll_offset(oversize.x / 2.0);
            }
            if oversize.y > 0.0 {
                scroll_area = scroll_area.vertical_scroll_offset(oversize.y / 2.0);
            }
            self.centered_zoom = self.viewer.zoom;
        }
        scroll_area.show(ui, |ui| {
            ui.vertical_centered(|ui| {
                let response = ui.add(
                    egui::Image::new((texture.id(), display_size)).sense(egui::Sense::click()),
                );
                if response.clicked()
                    && let Some(pointer) = response.interact_pointer_pos()
                    && let Some((width_pt, height_pt)) = self.page_dims_pt()
                {
                    let relative = (pointer - response.rect.min) / response.rect.size();
                    let x_pt = relative.x.clamp(0.0, 1.0) as f64 * width_pt;
                    let y_pt = relative.y.clamp(0.0, 1.0) as f64 * height_pt;
                    // Click-to-source: emit the reverse SyncTeX
                    // notification over the editor wire.
                    if let Some(message) =
                        self.app
                            .synctex_reverse_message(self.viewer.page + 1, x_pt, y_pt)
                    {
                        if let EditorMessage::Synctex { path, line, .. } = &message {
                            self.status = format!("syncTeX: {path}:{line}");
                        }
                        self.emit(&[message]);
                    } else {
                        self.status = format!("No syncTeX hit on page {}", self.viewer.page + 1);
                    }
                }
            });
        });
    }

    /// Page size in points for the current page, parsed from the XDV stream.
    fn page_dims_pt(&self) -> Option<(f64, f64)> {
        let artifact = self.viewer.last_artifact.as_ref()?;
        if !matches!(
            artifact.kind,
            oxipresso_engine_api::ArtifactKind::Xdv | oxipresso_engine_api::ArtifactKind::Dvi
        ) {
            return None;
        }
        let document = oxipresso_render::xdv::parse_xdv(&artifact.bytes, &mut |_| None).ok()?;
        let page = document.pages.get(self.viewer.page)?;
        Some((page.width_pt, page.height_pt))
    }

    /// Re-renders the current page when the displayed resolution demands
    /// more pixels than the cached texture has (zoom / window resize), so
    /// text stays crisp instead of upscaling a 96dpi bitmap.
    fn ensure_texture(&mut self, ctx: &egui::Context, available: egui::Vec2) {
        let Some(artifact) = self.viewer.last_artifact.clone() else {
            return;
        };
        let wanted = self.wanted_render_scale(ctx, available);
        let scale_changed =
            (wanted - self.rendered_scale).abs() / self.rendered_scale.max(0.01) > 0.15;
        if scale_changed {
            // Force a re-render at the new density (the key changes too).
            self.texture_key = None;
        }
        let key = page_texture_key(&artifact, self.viewer.page)
            ^ (wanted as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        if self.texture_key == Some(key) {
            return;
        }
        match self
            .renderer
            .render_page_scaled(&artifact, self.viewer.page, wanted)
        {
            Ok(page) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [page.width as usize, page.height as usize],
                    &page.pixels_rgba,
                );
                let handle = ctx.load_texture("page", image, egui::TextureOptions::LINEAR);
                // Logical page size in points: the texture holds
                // pt × (96/72) × wanted pixels.
                let logical =
                    egui::vec2(page.width as f32, page.height as f32) / (4.0 / 3.0 * wanted);
                self.rendered_size = Some(logical);
                self.rendered_scale = wanted;
                self.texture = Some(handle);
                self.texture_key = Some(key);
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    /// The pixel density the current zoom/fit actually needs, clamped to a
    /// re-render budget (≤4× the 96dpi base ≈ 384dpi, beyond which the blur
    /// is imperceptible and the raster cost dominates).
    fn wanted_render_scale(&self, ctx: &egui::Context, available: egui::Vec2) -> f32 {
        let ppp = ctx.pixels_per_point();
        let fit = match self.page_dims_pt() {
            Some((w_pt, h_pt)) => {
                let sx = available.x / w_pt as f32;
                let sy = available.y / h_pt as f32;
                match self.viewer.fit_mode {
                    oxipresso_viewer::FitMode::Page => sx.min(sy),
                    oxipresso_viewer::FitMode::Width => sx.min(4.0),
                }
            }
            None => 1.0,
        };
        (fit * self.viewer.zoom * ppp).clamp(1.0, 4.0)
    }
}

fn page_texture_key(artifact: &oxipresso_engine_api::DocumentArtifact, page: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in &artifact.bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash ^ (page as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

impl eframe::App for LivePreview {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_editor_wire();
        self.poll_document_watcher();
        // Keep the GUI responsive to the editor wire even when idle.
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("oxipresso-live-toolbar").show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("<").clicked() {
                    self.viewer.previous_page();
                }
                ui.label(format!(
                    "{}/{}",
                    self.viewer.page + 1,
                    self.viewer.page_count
                ));
                if ui.button(">").clicked() {
                    self.viewer.next_page();
                }
                if ui.button("-").clicked() {
                    self.viewer.adjust_zoom(-0.1);
                }
                if ui.button("+").clicked() {
                    self.viewer.adjust_zoom(0.1);
                }
                if ui.button("Fit").clicked() {
                    self.viewer.set_fit_mode(oxipresso_viewer::FitMode::Page);
                }
                if ui.button("1:1").clicked() {
                    self.viewer.zoom = 1.0;
                }
                ui.separator();
                ui.label(&self.status);
            });
        });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.page_image(ui);
        });
    }
}
