//! Real glyph rendering for XDV/DVI artifacts: parses the stream, resolves
//! font files through a [`FontResolver`], rasterizes glyphs with FreeType,
//! and composites them onto an RGBA page canvas.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use oxipresso_engine_api::{ArtifactKind, DocumentArtifact, EngineError, Result};

use crate::{RenderBackend, RenderedPage, ft, xdv};

/// Locates font files for the renderer. Implementations typically wrap a
/// TeX distribution lookup (e.g. kpsewhich) or system font directories.
pub trait FontResolver {
    /// Returns the font file bytes for `name` tried with each extension in
    /// order (e.g. `cmr10` + `["pfb", "ttf", "otf"]`), or `None`.
    fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>>;
}

const NATIVE_FONT_EXTENSIONS: &[&str] = &["otf", "ttf"];
const CLASSIC_FONT_EXTENSIONS: &[&str] = &["pfb", "ttf", "otf"];

pub struct XdvGlyphRenderBackend {
    /// Page rasterization scale in pixels per point (96 dpi = 4/3).
    pub px_per_pt: f64,
    resolver: RefCell<Box<dyn FontResolver>>,
    library: RefCell<Option<ft::FT_Library>>,
    faces: RefCell<HashMap<(String, u32), ft::FT_Face>>,
    font_files: RefCell<HashMap<String, Option<Rc<Vec<u8>>>>>,
    glyph_cache: RefCell<HashMap<(String, u32, u32, u32), Option<Rc<GrayBitmap>>>>,
    parsed: RefCell<Option<(u64, Rc<xdv::XdvDocument>)>>,
}

struct GrayBitmap {
    left: i64,
    top: i64,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl Drop for XdvGlyphRenderBackend {
    fn drop(&mut self) {
        if let Some(library) = self.library.borrow_mut().take() {
            for face in self.faces.borrow().values() {
                unsafe {
                    ft::FT_Done_Face(*face);
                }
            }
            unsafe {
                ft::FT_Done_FreeType(library);
            }
        }
    }
}

impl std::fmt::Debug for XdvGlyphRenderBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("XdvGlyphRenderBackend")
            .field("px_per_pt", &self.px_per_pt)
            .finish_non_exhaustive()
    }
}

impl XdvGlyphRenderBackend {
    pub fn new(resolver: Box<dyn FontResolver>) -> Self {
        Self {
            px_per_pt: 96.0 / 72.0,
            resolver: RefCell::new(resolver),
            library: RefCell::new(None),
            faces: RefCell::new(HashMap::new()),
            font_files: RefCell::new(HashMap::new()),
            glyph_cache: RefCell::new(HashMap::new()),
            parsed: RefCell::new(None),
        }
    }

    fn ensure_library(&self) -> Result<ft::FT_Library> {
        if let Some(library) = *self.library.borrow() {
            return Ok(library);
        }
        let mut library: ft::FT_Library = std::ptr::null_mut();
        let status = unsafe { ft::FT_Init_FreeType(&mut library) };
        if status != 0 || library.is_null() {
            return Err(EngineError::new(format!(
                "FT_Init_FreeType failed with status {status}"
            )));
        }
        *self.library.borrow_mut() = Some(library);
        Ok(library)
    }

    fn font_bytes(&self, name: &str, native: bool) -> Option<Rc<Vec<u8>>> {
        if let Some(cached) = self.font_files.borrow().get(name) {
            return cached.clone();
        }
        let extensions = if native {
            NATIVE_FONT_EXTENSIONS
        } else {
            CLASSIC_FONT_EXTENSIONS
        };
        let mut found = self
            .resolver
            .borrow_mut()
            .find_font_file(name, extensions)
            .map(Rc::new);
        if found.is_none() {
            let lowercase = name.to_lowercase();
            if lowercase != name {
                found = self
                    .resolver
                    .borrow_mut()
                    .find_font_file(&lowercase, extensions)
                    .map(Rc::new);
            }
        }
        self.font_files
            .borrow_mut()
            .insert(name.to_string(), found.clone());
        found
    }

    fn tfm_bytes(&self, name: &str) -> Option<Vec<u8>> {
        self.resolver.borrow_mut().find_font_file(name, &["tfm"])
    }

    fn face_for(&self, font: &xdv::XdvFont) -> Result<Option<ft::FT_Face>> {
        let Some(bytes) = self.font_bytes(&font.name, font.native) else {
            return Ok(None);
        };
        let key = (font.name.clone(), font.face_index);
        if let Some(face) = self.faces.borrow().get(&key) {
            return Ok(Some(*face));
        }
        let library = self.ensure_library()?;
        let mut face: ft::FT_Face = std::ptr::null_mut();
        let status = unsafe {
            ft::FT_New_Memory_Face(
                library,
                bytes.as_ptr(),
                bytes.len() as _,
                font.face_index as _,
                &mut face,
            )
        };
        if status != 0 || face.is_null() {
            return Err(EngineError::new(format!(
                "FT_New_Memory_Face failed for {} with status {status}",
                font.name
            )));
        }
        self.faces.borrow_mut().insert(key, face);
        Ok(Some(face))
    }

    fn rasterize(&self, font: &xdv::XdvFont, code: u32, size_px: u32) -> Option<Rc<GrayBitmap>> {
        let cache_key = (font.name.clone(), font.face_index, code, size_px);
        if let Some(cached) = self.glyph_cache.borrow().get(&cache_key) {
            return cached.clone();
        }
        let rendered = self.rasterize_uncached(font, code, size_px).map(Rc::new);
        self.glyph_cache
            .borrow_mut()
            .insert(cache_key, rendered.clone());
        rendered
    }

    fn rasterize_uncached(
        &self,
        font: &xdv::XdvFont,
        code: u32,
        size_px: u32,
    ) -> Option<GrayBitmap> {
        if size_px == 0 {
            return None;
        }
        let face = self.face_for(font).ok()??;
        unsafe {
            if ft::FT_Set_Pixel_Sizes(face, 0, size_px) != 0 {
                return None;
            }
            let mut glyph_index = ft::FT_Get_Char_Index(face, code);
            if glyph_index == 0 {
                // TeX Type1 fonts are indexed by char code directly.
                glyph_index = code;
            }
            if ft::FT_Load_Glyph(face, glyph_index, ft::FT_LOAD_DEFAULT) != 0 {
                return None;
            }
            let slot = ft::find_glyph_slot(face)?;
            if ft::FT_Render_Glyph(slot, ft::FT_RENDER_MODE_NORMAL) != 0 {
                return None;
            }
            let rendered = ft::read_rendered_bitmap(slot)?;
            Some(GrayBitmap {
                left: rendered.left,
                top: rendered.top,
                width: rendered.width,
                height: rendered.height,
                pixels: rendered.pixels,
            })
        }
    }

    fn parse_document(&self, artifact: &DocumentArtifact) -> Result<Rc<xdv::XdvDocument>> {
        let hash = fnv_hash(&artifact.bytes);
        if let Some((parsed_hash, document)) = self.parsed.borrow().as_ref()
            && *parsed_hash == hash
        {
            return Ok(document.clone());
        }
        let document = Rc::new(xdv::parse_xdv(&artifact.bytes, &mut |name| {
            self.tfm_bytes(name)
        })?);
        *self.parsed.borrow_mut() = Some((hash, document.clone()));
        Ok(document)
    }
}

fn fnv_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

fn blit_gray(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    bitmap: &GrayBitmap,
    left: i64,
    top: i64,
    rgba: u32,
) {
    let color_r = (rgba >> 24) & 0xff;
    let color_g = (rgba >> 16) & 0xff;
    let color_b = (rgba >> 8) & 0xff;
    for row in 0..bitmap.height as i64 {
        let y = top + row;
        if y < 0 || y >= canvas_height as i64 {
            continue;
        }
        for column in 0..bitmap.width as i64 {
            let x = left + column;
            if x < 0 || x >= canvas_width as i64 {
                continue;
            }
            let alpha =
                bitmap.pixels[(row as usize) * bitmap.width as usize + column as usize] as u32;
            if alpha == 0 {
                continue;
            }
            let offset = ((y as usize) * (canvas_width as usize) + x as usize) * 4;
            let inverse = 255 - alpha;
            canvas[offset] = ((color_r * alpha + u32::from(canvas[offset]) * inverse) / 255) as u8;
            canvas[offset + 1] =
                ((color_g * alpha + u32::from(canvas[offset + 1]) * inverse) / 255) as u8;
            canvas[offset + 2] =
                ((color_b * alpha + u32::from(canvas[offset + 2]) * inverse) / 255) as u8;
            canvas[offset + 3] = 255;
        }
    }
}

impl RenderBackend for XdvGlyphRenderBackend {
    fn page_count(&self, artifact: &DocumentArtifact) -> Result<usize> {
        if !matches!(artifact.kind, ArtifactKind::Xdv | ArtifactKind::Dvi) {
            return Ok(0);
        }
        Ok(self.parse_document(artifact)?.pages.len())
    }

    fn render_page(&self, artifact: &DocumentArtifact, page: usize) -> Result<RenderedPage> {
        if !matches!(artifact.kind, ArtifactKind::Xdv | ArtifactKind::Dvi) {
            return Err(EngineError::new(
                "XdvGlyphRenderBackend only accepts XDV/DVI artifacts",
            ));
        }
        let document = self.parse_document(artifact)?;
        let page_data = document
            .pages
            .get(page)
            .ok_or_else(|| EngineError::new("page index out of range"))?;
        let scale = self.px_per_pt;
        let width = (page_data.width_pt * scale).round().max(1.0) as u32;
        let height = (page_data.height_pt * scale).round().max(1.0) as u32;
        let mut canvas = vec![255u8; width as usize * height as usize * 4];

        // DVI origin: one inch from the top-left corner, y increases downward.
        let origin_px = 72.0 * scale;

        for element in &page_data.elements {
            match element {
                xdv::XdvElement::Rule {
                    x_pt,
                    y_pt,
                    w_pt,
                    h_pt,
                } => {
                    let x0 = (origin_px + x_pt * scale).floor() as i64;
                    let y0 = (origin_px + y_pt * scale).floor() as i64;
                    let w = (w_pt * scale).round().max(1.0) as i64;
                    let h = (h_pt * scale).round().max(1.0) as i64;
                    for row in y0..(y0 + h) {
                        if row < 0 || row >= height as i64 {
                            continue;
                        }
                        for column in x0..(x0 + w) {
                            if column < 0 || column >= width as i64 {
                                continue;
                            }
                            let offset = (row as usize * width as usize + column as usize) * 4;
                            canvas[offset] = 0;
                            canvas[offset + 1] = 0;
                            canvas[offset + 2] = 0;
                            canvas[offset + 3] = 255;
                        }
                    }
                }
                xdv::XdvElement::Glyphs { font_id, glyphs } => {
                    let Some(font) = document.fonts.get(font_id) else {
                        continue;
                    };
                    let size_px = (font.size_pt * scale).round().max(1.0) as u32;
                    let rgba = font.color_rgba.unwrap_or(0x000000ff) | 0xff;
                    for glyph in glyphs {
                        let Some(bitmap) = self.rasterize(font, glyph.code, size_px) else {
                            continue;
                        };
                        let pen_x = (origin_px + glyph.x_pt * scale).floor() as i64;
                        let pen_y = (origin_px + glyph.y_pt * scale).floor() as i64;
                        // FreeType bitmaps carry their bearing: bitmap_left is
                        // right of the pen, bitmap_top is above the pen.
                        blit_gray(
                            &mut canvas,
                            width,
                            height,
                            &bitmap,
                            pen_x + bitmap.left,
                            pen_y - bitmap.top,
                            rgba,
                        );
                    }
                }
            }
        }

        Ok(RenderedPage {
            index: page,
            width,
            height,
            pixels_rgba: canvas,
        })
    }
}
