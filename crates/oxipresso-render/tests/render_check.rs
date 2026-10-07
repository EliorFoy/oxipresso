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
    let page_index = std::env::var("OXI_RENDER_PAGE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    // Cache behavior measurements: (1) cold render, (2) same-artifact
    // re-render (page-cache hit expected), (3) re-render after an appended
    // trailing byte (same pages; page-cache hit expected), (4) a mutated
    // glyph code on the rendered page (page-cache miss; the glyph cache
    // should still make this far cheaper than the cold render).
    let t0 = std::time::Instant::now();
    let page = backend
        .render_page_scaled(&artifact, page_index, scale)
        .expect("render");
    println!("cold render: {}ms", t0.elapsed().as_millis());
    let t1 = std::time::Instant::now();
    let again = backend.render_page_scaled(&artifact, page_index, scale);
    println!("same-artifact re-render: {}ms", t1.elapsed().as_millis());
    drop(again);
    let t2 = std::time::Instant::now();
    let _ = backend.render_page_scaled(&artifact, page_index, scale);
    println!("third render: {}ms", t2.elapsed().as_millis());
    let mut mutated = artifact.clone();
    if let Some(pos) = mutated.bytes.iter().rposition(|&b| b == 0x8C) {
        mutated.bytes[pos - 1] = mutated.bytes[pos - 1].wrapping_add(1);
    }
    let t3 = std::time::Instant::now();
    let _ = backend.render_page_scaled(&mutated, page_index, scale);
    println!("mutated-artifact re-render: {}ms", t3.elapsed().as_millis());
    // Realistic edit-re-render: warm the glyph cache on the BASE artifact,
    // then render the EDITED artifact (a real insertion at the abstract) —
    // page digest changes, glyph codes mostly identical.
    if let Ok(base_path) = std::env::var("OXI_RENDER_XDV2") {
        if let Ok(base_bytes) = std::fs::read(&base_path) {
            let base = DocumentArtifact {
                kind: ArtifactKind::Xdv,
                bytes: base_bytes,
                source_name: Some("base.xdv".to_string()),
            };
            let tb = std::time::Instant::now();
            let _ = backend.render_page_scaled(&base, page_index, scale);
            println!("base render: {}ms", tb.elapsed().as_millis());
            let te = std::time::Instant::now();
            let _ = backend.render_page_scaled(&artifact, page_index, scale);
            println!(
                "edit re-render (warm glyph cache): {}ms",
                te.elapsed().as_millis()
            );
        }
    }
    let page = backend
        .render_page_scaled(&artifact, page_index, scale)
        .expect("render");
    println!("page {}x{} px", page.width, page.height);
    // Ink summary: the total dark pixels + the bottom-band share. A
    // document-tail marker (the auto-edit) shows up as the bottom-band ink
    // on an otherwise sparse last page.
    let dark: usize = page
        .pixels_rgba
        .chunks_exact(4)
        .filter(|p| p[0] < 128)
        .count();
    let h = page.height as usize;
    let stride = page.width as usize * 4;
    let bottom_start = h.saturating_sub(h / 6) * stride;
    let bottom_dark: usize = page.pixels_rgba[bottom_start..]
        .chunks_exact(4)
        .filter(|p| p[0] < 128)
        .count();
    println!(
        "dark total={} bottom_band={} (page index {})",
        dark, bottom_dark, page_index
    );
    let out_path = std::env::var("OXI_RENDER_PNG")
        .unwrap_or_else(|_| "F:/code/oxipresso/demo/render-check.png".to_string());
    let file = std::fs::File::create(&out_path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), page.width, page.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&page.pixels_rgba).unwrap();
    println!("written {out_path}");
}
