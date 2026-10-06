// Needs the freetype glyph backend.
#![cfg(feature = "freetype")]
//! Headless page render to a PNG + per-band ink report, for visual checks
//! (math glyphs, radicals, rules) without launching the GUI.

use oxipresso_engine_api::{ArtifactKind, DocumentArtifact};
use oxipresso_render::{RenderBackend, XdvGlyphRenderBackend};

struct CliResolver;
impl oxipresso_render::FontResolver for CliResolver {
    fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>> {
        // The engine records some fonts as RESOLVED PATHS (ctex CJK fonts
        // like C:/WINDOWS/fonts\simhei.ttf) — read them verbatim.
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
fn render_demo_page_to_png() {
    let path = std::env::var("OXI_RENDER_XDV")
        .unwrap_or_else(|_| "F:/code/oxipresso/demo/demo.xdv".to_string());
    if !std::path::Path::new(&path).is_file() {
        eprintln!("demo.xdv not found; skipping");
        return;
    }
    let bytes = std::fs::read(&path).unwrap();
    let artifact = DocumentArtifact {
        kind: ArtifactKind::Xdv,
        bytes,
        source_name: Some("demo.xdv".to_string()),
    };
    let backend = XdvGlyphRenderBackend::new(Box::new(CliResolver));
    let scale = std::env::var("OXI_RENDER_SCALE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(1.38);
    let page = backend
        .render_page_scaled(&artifact, 0, scale)
        .expect("render");
    println!("page {}x{} px", page.width, page.height);
    let out_path = "F:/code/oxipresso/demo/render-check.png";
    let file = std::fs::File::create(out_path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), page.width, page.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&page.pixels_rgba).unwrap();
    println!("written {out_path}");
}
