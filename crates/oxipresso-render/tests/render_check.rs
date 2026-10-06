// The classic-font probe needs the freetype glyph backend.
#![cfg(feature = "freetype")]

//! Renders the demo.xdv page 1 to a PNG + reports per-font ink so the math
//! rendering can be verified headlessly (no GUI needed).

use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_render::{RenderBackend, XdvGlyphRenderBackend};

struct CliResolver;
impl oxipresso_render::FontResolver for CliResolver {
    fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>> {
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
fn render_demo_page_to_png() {
    let path = if std::path::Path::new("F:/code/oxipresso/demo/gui-artifact.xdv").is_file() {
        "F:/code/oxipresso/demo/gui-artifact.xdv"
    } else {
        "F:/code/oxipresso/demo/demo.xdv"
    };
    if !std::path::Path::new(path).is_file() {
        eprintln!("demo.xdv not found; skipping");
        return;
    }
    let bytes = std::fs::read(path).unwrap();
    let artifact = DocumentArtifact {
        kind: ArtifactKind::Xdv,
        bytes,
        source_name: Some("demo.xdv".to_string()),
    };
    let backend = XdvGlyphRenderBackend::new(Box::new(CliResolver));
    let scale = std::env::var("OXI_RENDER_SCALE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.0);
    let page = if (scale - 1.0).abs() < f32::EPSILON {
        backend.render_page(&artifact, 0).expect("render")
    } else {
        backend
            .render_page_scaled(&artifact, 0, scale)
            .expect("scaled render")
    };
    println!("page {}x{} px", page.width, page.height);
    // Per-row ink profile: which vertical bands have ink (equations live in
    // the middle bands)?
    let stride = page.width as usize * 4;
    let rows = page.height as usize;
    let mut bands = std::collections::BTreeMap::new();
    for (y, row) in page.pixels_rgba.chunks_exact(stride).enumerate() {
        let dark = row.chunks_exact(4).filter(|p| p[0] < 100).count();
        if dark > 0 {
            let band = y / 40;
            *bands.entry(band).or_insert(0usize) += dark;
        }
    }
    println!("ink bands (y/40: dark px): {bands:?}");
    let out_path = "F:/code/oxipresso/demo/render-check.png";
    let file = std::fs::File::create(out_path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), page.width, page.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&page.pixels_rgba).unwrap();
    println!("written {out_path}");
}
