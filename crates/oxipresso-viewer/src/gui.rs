use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use eframe::egui;
use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_platform::{FileWatchEvent, FileWatcher, PollingFileWatcher, file_watcher};
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
    watcher: Option<PollingFileWatcher>,
    poll_interval: Duration,
    last_watch_check: Option<Instant>,
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
                true
            }
            Err(error) => {
                self.status = error.to_string();
                false
            }
        }
    }

    pub fn watch_path(&self) -> Option<&Path> {
        self.watcher.as_ref().map(FileWatcher::path)
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
        let Some(event) = self.watcher.as_mut().map(FileWatcher::poll) else {
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
        let Some(texture) = self.texture.as_ref() else {
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
                    let response = ui.add(egui::Image::new((texture.id(), display_size)));
                    if let Some(marker_rect) = self.paint_sync_marker(ui, response.rect) {
                        ui.scroll_to_rect(marker_rect, Some(egui::Align::Center));
                    }
                });
            });
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
