use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui;
use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_platform::{FileWatchEvent, FileWatcher, file_watcher};
use oxipresso_render::{AutoRenderBackend, RenderBackend, RenderedPage};

use crate::{FitMode, ViewerState, ViewerSyncPosition, load_artifact_from_path};

const TEX_POINT_SCALE: f32 = 65_536.0;
const DEFAULT_PAGE_WIDTH_PT: f32 = 612.0;
const DEFAULT_PAGE_HEIGHT_PT: f32 = 792.0;

#[derive(Debug, Clone)]
pub struct ViewerGuiOptions {
    pub title: String,
    pub initial_artifact: Option<DocumentArtifact>,
    pub watch_path: Option<PathBuf>,
    pub poll_interval: Duration,
}

impl Default for ViewerGuiOptions {
    fn default() -> Self {
        Self {
            title: "Oxipresso Viewer".to_string(),
            initial_artifact: None,
            watch_path: None,
            poll_interval: Duration::from_millis(500),
        }
    }
}

impl ViewerGuiOptions {
    pub fn with_artifact(artifact: DocumentArtifact) -> Self {
        Self {
            title: artifact
                .source_name
                .clone()
                .unwrap_or_else(|| "Oxipresso Viewer".to_string()),
            initial_artifact: Some(artifact),
            watch_path: None,
            poll_interval: Duration::from_millis(500),
        }
    }

    pub fn watching(path: PathBuf) -> Self {
        Self {
            title: path.to_string_lossy().to_string(),
            initial_artifact: load_artifact_from_path(&path).ok(),
            watch_path: Some(path),
            poll_interval: Duration::from_millis(500),
        }
    }
}

pub fn run_native_viewer(options: ViewerGuiOptions) -> eframe::Result<()> {
    let title = options.title.clone();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([520.0, 360.0]),
        ..Default::default()
    };

    eframe::run_native(
        &title,
        native_options,
        Box::new(move |_creation_context| Ok(Box::new(ViewerGuiApp::new(options)))),
    )
}

pub struct ViewerGuiApp {
    pub state: ViewerState,
    renderer: AutoRenderBackend,
    texture: Option<egui::TextureHandle>,
    texture_key: Option<TextureKey>,
    rendered_size: Option<egui::Vec2>,
    status: String,
    watcher: Option<Box<dyn FileWatcher>>,
    poll_interval: Duration,
    last_watch_check: Option<Instant>,
    /// Reverse-SyncTeX document loaded from the `.synctex` sidecar next to
    /// the artifact; enables click-to-source in the viewer.
    synctex: Option<oxipresso_synctex::SyncTexDocument>,
}

impl ViewerGuiApp {
    pub fn new(options: ViewerGuiOptions) -> Self {
        let mut app = Self {
            state: ViewerState::default(),
            renderer: AutoRenderBackend::default(),
            texture: None,
            texture_key: None,
            rendered_size: None,
            status: String::new(),
            watcher: options.watch_path.map(file_watcher),
            poll_interval: options.poll_interval,
            last_watch_check: None,
            synctex: None,
        };
        if let Some(artifact) = options.initial_artifact {
            app.load_artifact(artifact);
        } else if app.watcher.is_some() {
            app.status = "Waiting for artifact".to_string();
        }
        app
    }

    pub fn load_artifact(&mut self, artifact: DocumentArtifact) {
        let _ = self.try_load_artifact(artifact);
    }

    fn try_load_artifact(&mut self, artifact: DocumentArtifact) -> bool {
        match self
            .state
            .load_artifact_with_renderer(artifact, &self.renderer)
        {
            Ok(()) => {
                self.status.clear();
                self.texture = None;
                self.texture_key = None;
                self.rendered_size = None;
                self.reload_synctex_sidecar();
                true
            }
            Err(error) => {
                self.status = error.to_string();
                false
            }
        }
    }

    /// Loads the `.synctex` sidecar sitting next to the artifact (same stem),
    /// if any, enabling reverse SyncTeX (click-to-source) in the viewer.
    fn reload_synctex_sidecar(&mut self) {
        self.synctex = None;
        let Some(source) = self
            .state
            .last_artifact
            .as_ref()
            .and_then(|artifact| artifact.source_name.as_ref())
        else {
            return;
        };
        let sidecar = Path::new(source).with_extension("synctex");
        let Ok(bytes) = std::fs::read(sidecar) else {
            return;
        };
        let artifact = oxipresso_engine_api::SyncTexArtifact {
            bytes,
            compressed: false,
            source_name: None,
        };
        match oxipresso_synctex::parse_artifact(&artifact) {
            Ok(document) => self.synctex = Some(document),
            Err(error) => self.status = format!("syncTeX sidecar parse failed: {error}"),
        }
    }

    pub fn watch_path(&self) -> Option<&Path> {
        self.watcher.as_deref().map(FileWatcher::path)
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("<").on_hover_text("Previous page").clicked() {
                self.state.previous_page();
            }
            if ui.button(">").on_hover_text("Next page").clicked() {
                self.state.next_page();
            }
            ui.label(format!(
                "{}/{}",
                self.state.page.saturating_add(1).min(self.state.page_count),
                self.state.page_count
            ));

            ui.separator();
            if ui.button("-").on_hover_text("Zoom out").clicked() {
                self.state.adjust_zoom(-0.1);
            }
            ui.label(format!("{:.0}%", self.state.zoom * 100.0));
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                self.state.adjust_zoom(0.1);
            }

            ui.separator();
            ui.selectable_value(&mut self.state.fit_mode, FitMode::Page, "Fit page");
            ui.selectable_value(&mut self.state.fit_mode, FitMode::Width, "Fit width");

            ui.separator();
            ui.checkbox(&mut self.state.crop, "Crop");
            ui.checkbox(&mut self.state.invert, "Invert");
            ui.checkbox(&mut self.state.themed, "Theme");

            if let Some(path) = self.watch_path() {
                ui.separator();
                ui.label(format!("Watching {}", path.display()));
            }

            if !self.status.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(&self.status).color(egui::Color32::RED));
            }
        });
    }

    fn poll_watched_artifact(&mut self) {
        let Some(event) = self.watcher.as_mut().map(|watcher| watcher.poll()) else {
            return;
        };
        match event {
            FileWatchEvent::Unchanged => {}
            FileWatchEvent::Missing { path } => {
                self.status = format!("Waiting for {}", path.display());
            }
            FileWatchEvent::Changed { path, modified } => match load_artifact_from_path(&path) {
                Ok(artifact) => {
                    if self.try_load_artifact(artifact) {
                        if let Some(watcher) = self.watcher.as_mut() {
                            watcher.mark_clean(modified);
                        }
                    }
                }
                Err(error) => {
                    self.status = format!("Could not reload {}: {error}", path.display());
                }
            },
        }
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) {
        let Some(artifact) = self.state.last_artifact.as_ref() else {
            return;
        };
        if self.state.page_count == 0 {
            return;
        }

        let key = TextureKey {
            artifact_hash: artifact_hash(artifact),
            page: self.state.page,
            invert: self.state.invert,
        };
        if self.texture_key == Some(key) {
            return;
        }

        match self.renderer.render_page(artifact, self.state.page) {
            Ok(page) => {
                let image = color_image_from_page(page, self.state.invert);
                self.rendered_size = Some(egui::vec2(image.size[0] as f32, image.size[1] as f32));
                self.texture = Some(ctx.load_texture(
                    format!("oxipresso-page-{}-{}", key.artifact_hash, key.page),
                    image,
                    egui::TextureOptions::LINEAR,
                ));
                self.texture_key = Some(key);
                self.status.clear();
            }
            Err(error) => {
                self.status = error.to_string();
            }
        }
    }

    fn page_image(&mut self, ui: &mut egui::Ui) {
        self.ensure_texture(ui.ctx());
        // Clone the handle so the page closure can borrow &mut self for the
        // click handler (TextureHandle is an Arc).
        let Some(texture) = self.texture.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label("No document");
            });
            return;
        };
        let rendered_size = self.rendered_size.unwrap_or_else(|| texture.size_vec2());
        let available = ui.available_size();
        let fit_scale = match self.state.fit_mode {
            FitMode::Page => {
                let width_scale = available.x / rendered_size.x;
                let height_scale = available.y / rendered_size.y;
                width_scale.min(height_scale).min(1.0)
            }
            FitMode::Width => (available.x / rendered_size.x).min(4.0),
        };
        let display_size = rendered_size * fit_scale * self.state.zoom;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let response = ui.add(
                        egui::Image::new((texture.id(), display_size)).sense(egui::Sense::click()),
                    );
                    if response.clicked()
                        && let Some(pointer) = response.interact_pointer_pos()
                    {
                        let relative = (pointer - response.rect.min) / response.rect.size();
                        let fraction_x = relative.x.clamp(0.0, 1.0) as f64;
                        let fraction_y = relative.y.clamp(0.0, 1.0) as f64;
                        if let Some((width_pt, height_pt)) = self.page_dims_pt() {
                            self.apply_synctex_reverse(
                                fraction_x * width_pt,
                                fraction_y * height_pt,
                            );
                        }
                    }
                    if let Some(marker_rect) = self.paint_sync_marker(ui, response.rect) {
                        ui.scroll_to_rect(marker_rect, Some(egui::Align::Center));
                    }
                });
            });
    }

    /// Page size in points for the current page, from the XDV stream itself.
    fn page_dims_pt(&self) -> Option<(f64, f64)> {
        let artifact = self.state.last_artifact.as_ref()?;
        if !matches!(artifact.kind, ArtifactKind::Xdv | ArtifactKind::Dvi) {
            return None;
        }
        let document = oxipresso_render::xdv::parse_xdv(&artifact.bytes, &mut |_| None).ok()?;
        let page = document.pages.get(self.state.page)?;
        Some((page.width_pt, page.height_pt))
    }

    /// Click-to-source: resolves the clicked page point through reverse
    /// SyncTeX and pins the source location (marker + status line).
    fn apply_synctex_reverse(&mut self, x_pt: f64, y_pt: f64) {
        let page = self.state.page + 1; // SyncTeX pages are 1-based
        let Some(synctex) = self.synctex.as_ref() else {
            self.status = format!("No syncTeX sidecar loaded (page {page})");
            return;
        };
        let x_sp = (x_pt * 65_536.0) as i32;
        let y_sp = (y_pt * 65_536.0) as i32;
        match synctex.reverse_search_page_point(page, x_sp, y_sp) {
            Some(hit) => {
                self.status = format!(
                    "syncTeX: {}:{} (input {})",
                    hit.path, hit.line, hit.input_index
                );
                self.state.set_sync_position(ViewerSyncPosition {
                    page: self.state.page,
                    path: hit.path,
                    line: hit.line,
                    x: hit.x,
                    y: hit.y,
                    width: hit.width,
                    height: hit.height,
                    depth: hit.depth,
                });
            }
            None => self.status = format!("No syncTeX hit on page {page}"),
        }
    }

    fn paint_sync_marker(&self, ui: &egui::Ui, image_rect: egui::Rect) -> Option<egui::Rect> {
        let Some(position) = self.state.sync_position.as_ref() else {
            return None;
        };
        if position.page != self.state.page {
            return None;
        }
        let Some(center) = sync_marker_position(position, image_rect) else {
            return None;
        };
        let painter = ui.painter();
        let color = egui::Color32::from_rgb(45, 120, 255);
        painter.circle_stroke(center, 7.0, egui::Stroke::new(2.0, color));
        painter.line_segment(
            [
                center + egui::vec2(-11.0, 0.0),
                center + egui::vec2(11.0, 0.0),
            ],
            egui::Stroke::new(1.5, color),
        );
        painter.line_segment(
            [
                center + egui::vec2(0.0, -11.0),
                center + egui::vec2(0.0, 11.0),
            ],
            egui::Stroke::new(1.5, color),
        );
        Some(egui::Rect::from_center_size(center, egui::vec2(28.0, 28.0)))
    }
}

impl eframe::App for ViewerGuiApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.watcher.is_none() {
            return;
        }

        let now = Instant::now();
        let should_poll = self
            .last_watch_check
            .is_none_or(|last| now.duration_since(last) >= self.poll_interval);
        if should_poll {
            self.last_watch_check = Some(now);
            self.poll_watched_artifact();
        }
        ctx.request_repaint_after(self.poll_interval);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("oxipresso-viewer-toolbar").show_inside(ui, |ui| {
            self.toolbar(ui);
        });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.page_image(ui);
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextureKey {
    artifact_hash: u64,
    page: usize,
    invert: bool,
}

fn artifact_hash(artifact: &DocumentArtifact) -> u64 {
    let mut hasher = DefaultHasher::new();
    match artifact.kind {
        ArtifactKind::Pdf => 0_u8,
        ArtifactKind::Xdv => 1,
        ArtifactKind::Dvi => 2,
        ArtifactKind::Unknown => 3,
    }
    .hash(&mut hasher);
    artifact.source_name.hash(&mut hasher);
    artifact.bytes.len().hash(&mut hasher);
    artifact
        .bytes
        .iter()
        .take(256)
        .for_each(|byte| byte.hash(&mut hasher));
    artifact
        .bytes
        .iter()
        .rev()
        .take(256)
        .for_each(|byte| byte.hash(&mut hasher));
    hasher.finish()
}

fn color_image_from_page(page: RenderedPage, invert: bool) -> egui::ColorImage {
    let mut pixels = page.pixels_rgba;
    if invert {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[0] = 255 - pixel[0];
            pixel[1] = 255 - pixel[1];
            pixel[2] = 255 - pixel[2];
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([page.width as usize, page.height as usize], &pixels)
}

fn sync_marker_position(
    position: &ViewerSyncPosition,
    image_rect: egui::Rect,
) -> Option<egui::Pos2> {
    if !image_rect.is_finite() || image_rect.width() <= 0.0 || image_rect.height() <= 0.0 {
        return None;
    }
    let x_pt = position.x as f32 / TEX_POINT_SCALE;
    let y_pt = position.y as f32 / TEX_POINT_SCALE;
    let x = (x_pt / DEFAULT_PAGE_WIDTH_PT).clamp(0.0, 1.0);
    let y = (y_pt / DEFAULT_PAGE_HEIGHT_PT).clamp(0.0, 1.0);
    Some(egui::pos2(
        image_rect.left() + image_rect.width() * x,
        image_rect.top() + image_rect.height() * y,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn watching_options_can_start_before_file_exists() {
        let path = unique_temp_dir().join("out.pdf");
        let app = ViewerGuiApp::new(ViewerGuiOptions::watching(path.clone()));
        assert_eq!(app.watch_path(), Some(path.as_path()));
        assert_eq!(app.state.page_count, 0);
        assert!(app.status.contains("Waiting"));
    }

    #[test]
    fn polling_watch_path_loads_created_artifact() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.pdf");
        let mut app = ViewerGuiApp::new(ViewerGuiOptions::watching(path.clone()));
        std::fs::write(&path, one_page_pdf()).unwrap();

        app.poll_watched_artifact();

        assert_eq!(app.state.page_count, 1);
        assert!(app.status.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_watch_reload_keeps_previous_artifact() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.pdf");
        std::fs::write(&path, one_page_pdf()).unwrap();
        let mut app = ViewerGuiApp::new(ViewerGuiOptions::watching(path.clone()));
        assert_eq!(app.state.page_count, 1);

        std::fs::write(&path, b"not a pdf").unwrap();
        if let Some(watcher) = app.watcher.as_mut() {
            watcher.mark_clean(None);
        }
        app.poll_watched_artifact();

        assert_eq!(app.state.page_count, 1);
        assert!(app.status.contains("%PDF-"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sync_marker_position_maps_tex_points_into_image_rect() {
        let position = ViewerSyncPosition {
            page: 0,
            path: "main.tex".to_string(),
            line: 5,
            x: (306.0 * TEX_POINT_SCALE) as i32,
            y: (396.0 * TEX_POINT_SCALE) as i32,
            width: None,
            height: None,
            depth: None,
        };
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(612.0, 792.0));
        let marker = sync_marker_position(&position, rect).unwrap();
        assert_eq!(marker, egui::pos2(316.0, 416.0));
    }

    fn one_page_pdf() -> &'static [u8] {
        br#"%PDF-1.7
1 0 obj
<< /Type /Pages /Count 1 >>
endobj
2 0 obj
<< /Type /Page /Parent 1 0 R >>
endobj
"#
    }

    fn unique_temp_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oxipresso-viewer-gui-test-{nonce}"))
    }
}
