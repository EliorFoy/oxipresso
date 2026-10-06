// Needs the freetype glyph backend.
#![cfg(feature = "freetype")]
//! Linux headless render check: the real engine's XDV -> PNG + ink bands.
//! Self-skips unless $HOME/demo-linux.xdv exists (produced by
//! `oxipresso -test-initialize demo.tex` on a Linux host with the real
//! engine and a TeX distribution for kpsewhich).

use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_render::{RenderBackend, XdvGlyphRenderBackend};

struct LinuxResolver;
impl oxipresso_render::FontResolver for LinuxResolver {
    fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>> {
        let normalized = name.replace('\\', "/");
        let direct = std::path::PathBuf::from(&normalized);
        if direct.is_file() {
            return std::fs::read(&direct).ok();
        }
        for extension in extensions {
            let out = std::process::Command::new("kpsewhich")
                .arg(format!("{name}.{extension}"))
                .output()
                .ok()?;
            if !out.status.success() {
                continue;
            }
            let line = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()?
                .trim()
                .to_string();
            if line.is_empty() {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&line) {
                return Some(bytes);
            }
        }
        None
    }
}

#[test]
fn render_linux_page() {
    let path = std::env::var("HOME").unwrap() + "/demo-linux.xdv";
    if !std::path::Path::new(&path).is_file() {
        eprintln!("demo-linux.xdv not found; skipping");
        return;
    }
    let bytes = std::fs::read(&path).expect("artifact");
    let artifact = DocumentArtifact {
        kind: ArtifactKind::Xdv,
        bytes,
        source_name: Some("demo-linux.xdv".into()),
    };
    let backend = XdvGlyphRenderBackend::new(Box::new(LinuxResolver));
    let page = backend
        .render_page_scaled(&artifact, 0, 1.38)
        .expect("render");
    println!("page {}x{}", page.width, page.height);
    let stride = page.width as usize * 4;
    let mut bands = std::collections::BTreeMap::new();
    for (y, row) in page.pixels_rgba.chunks_exact(stride).enumerate() {
        let dark = row.chunks_exact(4).filter(|p| p[0] < 100).count();
        if dark > 0 {
            *bands.entry(y / 40).or_insert(0usize) += dark;
        }
    }
    let total: usize = bands.values().sum();
    println!("ink bands: {bands:?}");
    println!("total dark: {total}");
    assert!(total > 2000, "linux render lost ink: {total}");
}
