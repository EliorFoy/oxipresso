// The classic-font probe needs the freetype glyph backend.
#![cfg(feature = "freetype")]

//! Math-glyph diagnosis: walk the demo.xdv classic-font glyph runs, resolve
//! each font's AFM (the encoding vector: charcode -> glyph name), map through
//! FT_Get_Name_Index, and report per-code glyph index + rendered bitmap size —
//! pinpointing exactly which layer loses the math glyphs.

use oxipresso_render::FontResolver;
use oxipresso_render::xdv::parse_xdv;

#[test]
fn probe_math_glyph_layers() {
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
    let mut classic_codes: std::collections::BTreeMap<u32, Vec<u32>> =
        std::collections::BTreeMap::new();
    for page in &doc.pages {
        for element in &page.elements {
            if let oxipresso_render::xdv::XdvElement::Glyphs {
                font_id, glyphs, ..
            } = element
            {
                if let Some(font) = doc.fonts.get(font_id)
                    && !font.native
                {
                    let entry = classic_codes.entry(*font_id).or_default();
                    for glyph in glyphs.iter() {
                        entry.push(glyph.code);
                    }
                }
            }
        }
    }
    println!("== classic glyph codes (ALL per font) ==");
    for (id, codes) in &classic_codes {
        let name = doc.fonts.get(id).map(|f| f.name.as_str()).unwrap_or("?");
        println!("  font {id} ({name}): {codes:?}");
    }

    println!("== afm + freetype layer ==");
    for (id, codes) in &classic_codes {
        let Some(font) = doc.fonts.get(id) else {
            continue;
        };
        let name = font.name.clone();
        let Some(afm_bytes) = CliResolver.find_afm(&name) else {
            println!("  font {id} ({name}): AFM RESOLUTION FAILED");
            continue;
        };
        let table = oxipresso_render::parse_afm_charmetrics(&afm_bytes).unwrap_or_default();
        println!(
            "  font {id} ({name}): afm {} B, {} metrics",
            afm_bytes.len(),
            table.len()
        );
        let Some(file_bytes) = CliResolver.find_font_file(&name, &["pfb", "ttf", "otf"]) else {
            println!("    FONT FILE RESOLUTION FAILED");
            continue;
        };
        unsafe {
            let mut library: oxipresso_render::ft::FT_Library = std::ptr::null_mut();
            if oxipresso_render::ft::FT_Init_FreeType(&mut library) != 0 {
                println!("    FT_Init FAILED");
                continue;
            }
            let mut face: oxipresso_render::ft::FT_Face = std::ptr::null_mut();
            if oxipresso_render::ft::FT_New_Memory_Face(
                library,
                file_bytes.as_ptr(),
                file_bytes.len() as std::os::raw::c_long,
                0,
                &mut face,
            ) != 0
            {
                println!("    FT_New_Memory_Face FAILED");
                continue;
            }
            for code in codes.iter() {
                let via_afm = table.get(code).map(|n| {
                    let cname = std::ffi::CString::new(n.as_str()).unwrap();
                    (
                        n.clone(),
                        oxipresso_render::ft::FT_Get_Name_Index(face, cname.as_ptr()),
                    )
                });
                match via_afm {
                    Some((glyph_name, gid)) if gid != 0 => {
                        oxipresso_render::ft::FT_Set_Pixel_Sizes(face, 0, 32);
                        oxipresso_render::ft::FT_Load_Glyph(
                            face,
                            gid,
                            oxipresso_render::ft::FT_LOAD_DEFAULT,
                        );
                        if let Some(slot) = oxipresso_render::ft::find_glyph_slot(face) {
                            oxipresso_render::ft::FT_Render_Glyph(
                                slot,
                                oxipresso_render::ft::FT_RENDER_MODE_NORMAL,
                            );
                        }
                        let bitmap = oxipresso_render::ft::find_glyph_slot(face)
                            .and_then(oxipresso_render::ft::read_rendered_bitmap)
                            .map(|b| format!("{}x{}", b.width, b.height))
                            .unwrap_or_else(|| "no-bitmap".to_string());
                        println!(
                            "    code {code} -> name '{glyph_name}' -> gid {gid} rendered={bitmap}"
                        );
                    }
                    Some((glyph_name, gid)) => {
                        println!(
                            "    code {code} -> name '{glyph_name}' -> gid 0 (NAME NOT FOUND)"
                        );
                    }
                    None => {
                        println!("    code {code} -> NOT IN AFM");
                    }
                }
            }
        }
    }
}

struct CliResolver;
impl FontResolver for CliResolver {
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

impl CliResolver {
    fn find_afm(&mut self, name: &str) -> Option<Vec<u8>> {
        self.find_font_file(name, &["afm"])
    }
}
