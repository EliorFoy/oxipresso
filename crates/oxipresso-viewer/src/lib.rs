use std::path::Path;

use oxipresso_engine_api::{ArtifactKind, DocumentArtifact, Result};
use oxipresso_render::RenderBackend;

#[cfg(feature = "gui")]
mod gui;

#[cfg(feature = "gui")]
pub use gui::{ViewerGuiApp, ViewerGuiOptions, run_native_viewer};

pub fn load_artifact_from_path(path: &Path) -> std::io::Result<DocumentArtifact> {
    let bytes = std::fs::read(path)?;
    Ok(DocumentArtifact {
        kind: artifact_kind_from_path_and_bytes(path, &bytes),
        bytes,
        source_name: Some(path.to_string_lossy().to_string()),
    })
}

pub fn artifact_kind_from_path_and_bytes(path: &Path, bytes: &[u8]) -> ArtifactKind {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pdf") => ArtifactKind::Pdf,
        Some("xdv") => ArtifactKind::Xdv,
        Some("dvi") => ArtifactKind::Dvi,
        _ if bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|start| bytes[start..].starts_with(b"%PDF-")) =>
        {
            ArtifactKind::Pdf
        }
        _ => ArtifactKind::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Width,
    Page,
}

#[derive(Debug, Clone)]
pub struct ViewerState {
    pub page: usize,
    pub page_count: usize,
    pub zoom: f32,
    pub fit_mode: FitMode,
    pub crop: bool,
    pub invert: bool,
    pub themed: bool,
    pub last_artifact: Option<DocumentArtifact>,
    pub sync_position: Option<ViewerSyncPosition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerSyncPosition {
    pub page: usize,
    pub path: String,
    pub line: usize,
    pub x: i32,
    pub y: i32,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub depth: Option<i32>,
}

impl Default for ViewerState {
    fn default() -> Self {
        Self {
            page: 0,
            page_count: 0,
            zoom: 1.0,
            fit_mode: FitMode::Page,
            crop: false,
            invert: false,
            themed: true,
            last_artifact: None,
            sync_position: None,
        }
    }
}

impl ViewerState {
    pub fn set_artifact(&mut self, artifact: DocumentArtifact, page_count: usize) {
        self.last_artifact = Some(artifact);
        self.page_count = page_count;
        if self.page >= self.page_count {
            self.page = self.page_count.saturating_sub(1);
        }
    }

    pub fn load_artifact_with_renderer(
        &mut self,
        artifact: DocumentArtifact,
        renderer: &dyn RenderBackend,
    ) -> Result<()> {
        let page_count = renderer.page_count(&artifact)?;
        self.set_artifact(artifact, page_count);
        Ok(())
    }

    pub fn next_page(&mut self) {
        if self.page + 1 < self.page_count {
            self.page += 1;
        }
    }
    pub fn previous_page(&mut self) {
        self.page = self.page.saturating_sub(1);
    }
    pub fn set_page(&mut self, page: usize) {
        if self.page_count == 0 {
            self.page = 0;
        } else {
            self.page = page.min(self.page_count - 1);
        }
    }
    pub fn set_sync_position(&mut self, position: ViewerSyncPosition) {
        self.set_page(position.page);
        self.sync_position = Some(position);
    }
    pub fn set_fit_mode(&mut self, fit_mode: FitMode) {
        self.fit_mode = fit_mode;
    }
    pub fn adjust_zoom(&mut self, delta: f32) {
        self.zoom = (self.zoom + delta).clamp(0.1, 8.0);
    }
    pub fn toggle_crop(&mut self) {
        self.crop = !self.crop;
    }
    pub fn toggle_invert(&mut self) {
        self.invert = !self.invert;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxipresso_engine_api::ArtifactKind;
    use oxipresso_render::PdfMetadataRenderBackend;

    #[test]
    fn page_navigation_is_bounded() {
        let mut state = ViewerState {
            page_count: 2,
            ..ViewerState::default()
        };
        state.next_page();
        state.next_page();
        assert_eq!(state.page, 1);
        state.previous_page();
        state.previous_page();
        assert_eq!(state.page, 0);
        state.set_page(8);
        assert_eq!(state.page, 1);
    }

    #[test]
    fn navigation_and_zoom_are_bounded_on_empty_document() {
        // The CLI holds a page_count 0 ViewerState until the first artifact
        // loads; an editor could send next/prev/zoom before then. These must
        // never panic or wrap.
        let mut state = ViewerState::default(); // page_count 0
        state.next_page();
        state.previous_page();
        state.set_page(5);
        assert_eq!(state.page, 0, "no pages: page stays 0, no underflow/panic");
        // Zoom saturates at both clamps regardless of repeated adjustment.
        for _ in 0..50 {
            state.adjust_zoom(1.0);
        }
        assert_eq!(state.zoom, 8.0, "zoom clamped at max");
        for _ in 0..100 {
            state.adjust_zoom(-1.0);
        }
        assert_eq!(state.zoom, 0.1, "zoom clamped at min");
    }

    #[test]
    fn loading_artifact_updates_page_count_and_clamps_page() {
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Pdf,
            bytes: br#"%PDF-1.7
1 0 obj
<< /Type /Pages /Count 1 >>
endobj
2 0 obj
<< /Type /Page /Parent 1 0 R >>
endobj
"#
            .to_vec(),
            source_name: Some("main.pdf".to_string()),
        };
        let mut state = ViewerState {
            page: 8,
            ..ViewerState::default()
        };
        state
            .load_artifact_with_renderer(artifact.clone(), &PdfMetadataRenderBackend)
            .unwrap();
        assert_eq!(state.page_count, 1);
        assert_eq!(state.page, 0);
        assert_eq!(state.last_artifact, Some(artifact));
    }

    #[test]
    fn detects_artifact_kind_from_extension_or_pdf_header() {
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("main.pdf"), b""),
            ArtifactKind::Pdf
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("main.xdv"), b""),
            ArtifactKind::Xdv
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("main.dvi"), b""),
            ArtifactKind::Dvi
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("main.bin"), b"\n%PDF-1.7\n"),
            ArtifactKind::Pdf
        );
        assert_eq!(
            artifact_kind_from_path_and_bytes(Path::new("main.bin"), b"unknown"),
            ArtifactKind::Unknown
        );
    }
}
