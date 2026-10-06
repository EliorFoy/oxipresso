//! Glyph/rule diagnosis: dump fonts, glyph runs (with positions) and rules
//! of a real engine-produced XDV. Self-skips unless the file exists.

use oxipresso_render::xdv::parse_xdv;

#[test]
fn dump_demo_xdv_fonts_and_glyph_codes() {
    let path = "F:/code/oxipresso/demo/demo.xdv";
    if !std::path::Path::new(path).is_file() {
        eprintln!("demo.xdv not found; skipping");
        return;
    }
    let bytes = std::fs::read(path).unwrap();
    let mut tfm_lookup = |name: &str| -> Option<Vec<u8>> {
        let out = std::process::Command::new("kpsewhich")
            .arg(format!("{name}.tfm"))
            .output()
            .ok()?;
        let line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .to_string();
        std::fs::read(&line).ok()
    };
    let doc = parse_xdv(&bytes, &mut tfm_lookup).expect("parse");
    println!("== fonts == ({} entries)", doc.fonts.len());
    for (id, font) in &doc.fonts {
        println!(
            "  id={id} native={} name={} face={} size={:.2}pt",
            font.native, font.name, font.face_index, font.size_pt
        );
    }
    println!("== pages ==");
    for page in &doc.pages {
        println!(
            "  page {} {:.1}x{:.1}pt, {} elements",
            page.page_number,
            page.width_pt,
            page.height_pt,
            page.elements.len()
        );
        for element in &page.elements {
            match element {
                oxipresso_render::xdv::XdvElement::Rule {
                    x_pt,
                    y_pt,
                    w_pt,
                    h_pt,
                } => {
                    println!("  RULE at ({x_pt:.1},{y_pt:.1}) size {w_pt:.1}x{h_pt:.1}pt");
                }
                oxipresso_render::xdv::XdvElement::Glyphs {
                    font_id, glyphs, ..
                } => {
                    let codes: Vec<String> = glyphs.iter().map(|g| g.code.to_string()).collect();
                    let sample = if codes.len() > 12 {
                        format!("{} ... (+{})", codes[..12].join(","), codes.len() - 12)
                    } else {
                        codes.join(",")
                    };
                    let first = glyphs
                        .first()
                        .map(|g| format!("({:.1},{:.1})", g.x_pt, g.y_pt))
                        .unwrap_or_default();
                    println!(
                        "  font {font_id} first{first}: {} glyphs [{sample}]",
                        glyphs.len()
                    );
                }
                _ => {}
            }
        }
    }
}
