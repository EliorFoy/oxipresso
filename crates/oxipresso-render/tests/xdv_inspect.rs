//! Glyph-identity diagnosis: dump the fonts and glyph codes of a real
//! engine-produced XDV so the viewer's rendering can be checked against the
//! engine's expectations. Self-skips unless the file exists.

use oxipresso_render::xdv::parse_xdv;

#[test]
fn dump_demo_xdv_fonts_and_glyph_codes() {
    let path = if std::path::Path::new("F:/code/oxipresso/demo/cn-current.xdv").is_file() {
        "F:/code/oxipresso/demo/cn-current.xdv"
    } else {
        "F:/code/oxipresso/demo/demo.xdv"
    };
    if !std::path::Path::new(path).is_file() {
        eprintln!("demo.xdv not found; skipping");
        return;
    }
    let bytes = std::fs::read(path).unwrap();
    let mut tfm_lookup = |name: &str| -> Option<Vec<u8>> {
        // resolve via kpsewhich (same source the engine used)
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
    // opcode histogram over the raw stream for the font-def family
    let mut hist = std::collections::BTreeMap::new();
    for b in bytes.iter() {
        if (243..=254).contains(b) {
            *hist.entry(*b).or_insert(0usize) += 1;
        }
    }
    println!("== opcode histogram (243..=254) ==");
    for (op, n) in &hist {
        println!("  op {op}: {n}x");
    }
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
            if let oxipresso_render::xdv::XdvElement::Glyphs {
                font_id, glyphs, ..
            } = element
            {
                let codes: Vec<String> = glyphs.iter().map(|g| g.code.to_string()).collect();
                let sample = if codes.len() > 24 {
                    format!("{} ... (+{})", codes[..24].join(","), codes.len() - 24)
                } else {
                    codes.join(",")
                };
                println!("  font {font_id}: {} glyphs [{}]", glyphs.len(), sample);
            }
        }
    }
}
