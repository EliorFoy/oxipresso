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

/// Locates image files referenced by `pdf:image` specials. Implementations
/// typically resolve the path against the document directory (the special
/// carries the path exactly as written in the TeX source).
pub trait ImageLoader {
    /// Returns the image file bytes for `path`, or `None`.
    fn find_image_file(&mut self, path: &str) -> Option<Vec<u8>>;
}

/// Default no-op loader: images render as nothing.
struct NullImageLoader;

impl ImageLoader for NullImageLoader {
    fn find_image_file(&mut self, _path: &str) -> Option<Vec<u8>> {
        None
    }
}

/// A decoded RGBA image.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// RGBA8, row-major.
    pub pixels: Vec<u8>,
}

pub struct XdvGlyphRenderBackend {
    /// Page rasterization scale in pixels per point (96 dpi = 4/3).
    pub px_per_pt: f64,
    resolver: RefCell<Box<dyn FontResolver>>,
    image_loader: RefCell<Box<dyn ImageLoader>>,
    library: RefCell<Option<ft::FT_Library>>,
    faces: RefCell<HashMap<(String, u32), ft::FT_Face>>,
    font_files: RefCell<HashMap<String, Option<Rc<Vec<u8>>>>>,
    glyph_cache: RefCell<HashMap<(String, u32, u32, u32, u64), Option<Rc<GrayBitmap>>>>,
    image_cache: RefCell<HashMap<String, Option<Rc<DecodedImage>>>>,
    parsed: RefCell<Option<(u64, Rc<xdv::XdvDocument>)>>,
    /// Rendered pages keyed by XDV page content digest. Pages whose digest
    /// matches a previous rebuild are reused without re-rendering, which is
    /// the renderer-side half of the incremental rebuild model.
    page_cache: RefCell<HashMap<u64, RenderedPage>>,
    #[cfg(test)]
    render_misses: std::cell::Cell<usize>,
}

const PAGE_CACHE_CAPACITY: usize = 64;

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
        Self::with_image_loader(resolver, Box::new(NullImageLoader))
    }

    pub fn with_image_loader(
        resolver: Box<dyn FontResolver>,
        image_loader: Box<dyn ImageLoader>,
    ) -> Self {
        Self {
            px_per_pt: 96.0 / 72.0,
            resolver: RefCell::new(resolver),
            image_loader: RefCell::new(image_loader),
            library: RefCell::new(None),
            faces: RefCell::new(HashMap::new()),
            font_files: RefCell::new(HashMap::new()),
            glyph_cache: RefCell::new(HashMap::new()),
            image_cache: RefCell::new(HashMap::new()),
            parsed: RefCell::new(None),
            page_cache: RefCell::new(HashMap::new()),
            #[cfg(test)]
            render_misses: std::cell::Cell::new(0),
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
        // The transform fingerprint keeps slanted/extended variants out of the
        // upright cache slots.
        let transform_key = font.slant.to_bits()
            ^ font.extend.to_bits().rotate_left(32)
            ^ font.embolden.to_bits().rotate_left(16);
        let cache_key = (
            font.name.clone(),
            font.face_index,
            code,
            size_px,
            transform_key,
        );
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
            let Some(slot) = ft::find_glyph_slot(face) else {
                return None;
            };
            if ft::FT_Render_Glyph(slot, ft::FT_RENDER_MODE_NORMAL) != 0 {
                return None;
            }
            let rendered = ft::read_rendered_bitmap(slot)?;
            let bitmap = GrayBitmap {
                left: rendered.left,
                top: rendered.top,
                width: rendered.width,
                height: rendered.height,
                pixels: rendered.pixels,
            };
            // Apply XDV extend/slant as bitmap post-processing. This avoids
            // FT_Set_Transform, whose MSVC-specific FT_Pos size (4 bytes, not
            // 8) invalidates every struct offset we rely on.
            let bitmap = apply_extend(bitmap, font.extend);
            let bitmap = apply_slant(bitmap, font.slant);
            let bitmap = apply_embolden(bitmap, font.embolden);
            Some(bitmap)
        }
    }

    /// Loads and decodes an image referenced by a `pdf:image` special, with
    /// per-path caching. Currently supports PNG (the format `\XeTeXpicfile`
    /// and `graphicx` most commonly feed XeTeX on this path).
    fn decode_image(&self, path: &str) -> Option<Rc<DecodedImage>> {
        if let Some(cached) = self.image_cache.borrow().get(path) {
            return cached.clone();
        }
        let bytes = self.image_loader.borrow_mut().find_image_file(path)?;
        let decoded = decode_png(&bytes);
        self.image_cache
            .borrow_mut()
            .insert(path.to_string(), decoded.clone());
        decoded
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

/// Decodes PNG bytes into RGBA. Returns `None` for undecodable input.
fn decode_png(bytes: &[u8]) -> Option<Rc<DecodedImage>> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let buffer_size = reader.output_buffer_size()?;
    let mut buffer = vec![0u8; buffer_size];
    let info = reader.next_frame(&mut buffer).ok()?;
    let width = info.width;
    let height = info.height;
    let pixels = match info.color_type {
        png::ColorType::Rgba => buffer[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            for chunk in buffer[..info.buffer_size()].chunks_exact(3) {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xff]);
            }
            rgba
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            for value in &buffer[..info.buffer_size()] {
                rgba.extend_from_slice(&[*value, *value, *value, 0xff]);
            }
            rgba
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            for pair in buffer[..info.buffer_size()].chunks_exact(2) {
                rgba.extend_from_slice(&[pair[0], pair[0], pair[0], pair[1]]);
            }
            rgba
        }
        _ => return None,
    };
    Some(Rc::new(DecodedImage {
        width,
        height,
        pixels,
    }))
}

/// Composites a decoded RGBA image onto the page canvas, stretched to
/// `dest_w × dest_h` pixels at `dest_x/dest_y` (nearest-neighbor sampling).
fn blit_image(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    image: &DecodedImage,
    dest_x: i64,
    dest_y: i64,
    dest_w: i64,
    dest_h: i64,
) {
    if dest_w <= 0 || dest_h <= 0 || image.width == 0 || image.height == 0 {
        return;
    }
    for row in 0..dest_h {
        let page_y = dest_y + row;
        if page_y < 0 || page_y >= canvas_height as i64 {
            continue;
        }
        let source_y = (row * image.height as i64 / dest_h) as u32;
        for column in 0..dest_w {
            let page_x = dest_x + column;
            if page_x < 0 || page_x >= canvas_width as i64 {
                continue;
            }
            let source_x = (column * image.width as i64 / dest_w) as u32;
            let source = (source_y as usize * image.width as usize + source_x as usize) * 4;
            let alpha = image.pixels[source + 3] as u32;
            let dest = (page_y as usize * canvas_width as usize + page_x as usize) * 4;
            if alpha >= 255 {
                canvas[dest] = image.pixels[source];
                canvas[dest + 1] = image.pixels[source + 1];
                canvas[dest + 2] = image.pixels[source + 2];
                canvas[dest + 3] = 255;
            } else if alpha > 0 {
                // Source-over blend against the opaque white page.
                let inverse = 255 - alpha;
                for channel in 0..3 {
                    let source_value = image.pixels[source + channel] as u32;
                    let dest_value = canvas[dest + channel] as u32;
                    canvas[dest + channel] =
                        ((source_value * alpha + dest_value * inverse) / 255) as u8;
                }
                canvas[dest + 3] = 255;
            }
        }
    }
}

/// Scales a gray bitmap horizontally by `factor` (XDV extend).
fn apply_extend(bitmap: GrayBitmap, factor: f64) -> GrayBitmap {
    if (factor - 1.0).abs() < 0.01 || bitmap.width == 0 || bitmap.height == 0 {
        return bitmap;
    }
    let new_width = ((bitmap.width as f64) * factor).round().max(1.0) as u32;
    let mut pixels = vec![0u8; (new_width * bitmap.height) as usize];
    for row in 0..bitmap.height {
        for col in 0..new_width {
            let source_x = ((col as f64 + 0.5) / factor) as u32;
            let source_x = source_x.min(bitmap.width - 1);
            pixels[(row * new_width + col) as usize] =
                bitmap.pixels[(row * bitmap.width + source_x) as usize];
        }
    }
    GrayBitmap {
        left: ((bitmap.left as f64) * factor).round() as i64,
        top: bitmap.top,
        width: new_width,
        height: bitmap.height,
        pixels,
    }
}

/// Shears a gray bitmap horizontally by `slant` (XDV slant: x' = x + slant*y
/// where y grows upward from the baseline, matching italic typography).
fn apply_slant(bitmap: GrayBitmap, slant: f64) -> GrayBitmap {
    if slant.abs() < 0.01 || bitmap.width == 0 || bitmap.height == 0 {
        return bitmap;
    }
    let shift_per_row = slant; // positive slant shifts top rows to the right
    let max_shift = (shift_per_row * (bitmap.height as f64 - 1.0)).ceil() as i64;
    let extra = max_shift.max(0);
    let new_width = bitmap.width + extra as u32;
    let mut pixels = vec![0u8; (new_width * bitmap.height) as usize];
    for row_from_top in 0..bitmap.height {
        let baseline_distance = (bitmap.height - 1 - row_from_top) as f64;
        let shift = (shift_per_row * baseline_distance).round() as i64;
        let dest_x = shift + extra; // extra shifts everything right for positive slant
        for col in 0..bitmap.width {
            let source = bitmap.pixels[(row_from_top * bitmap.width + col) as usize];
            if source == 0 {
                continue;
            }
            let dest = dest_x + col as i64;
            if dest >= 0 && (dest as u32) < new_width {
                let index = row_from_top as usize * new_width as usize + dest as usize;
                pixels[index] = source;
            }
        }
    }
    GrayBitmap {
        left: bitmap.left - extra,
        top: bitmap.top,
        width: new_width,
        height: bitmap.height,
        pixels,
    }
}

/// Dilates a gray bitmap by `strength` pixels in each direction (XDV
/// embolden). Each output pixel takes the max of all source pixels within
/// the strength radius, creating a visually bolder glyph.
fn apply_embolden(bitmap: GrayBitmap, strength: f64) -> GrayBitmap {
    let radius = (strength * 8.0).round() as i64; // scale factor for visible effect
    if radius < 1 || bitmap.width == 0 || bitmap.height == 0 {
        return bitmap;
    }
    let new_width = bitmap.width + radius as u32 * 2;
    let new_height = bitmap.height + radius as u32 * 2;
    let mut pixels = vec![0u8; (new_width * new_height) as usize];
    for row in 0..bitmap.height as i64 {
        for col in 0..bitmap.width as i64 {
            let source = bitmap.pixels[(row as usize) * bitmap.width as usize + col as usize];
            if source == 0 {
                continue;
            }
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let dr = (row + radius + dy) as usize;
                    let dc = (col + radius + dx) as usize;
                    let index = dr * new_width as usize + dc;
                    pixels[index] = pixels[index].max(source);
                }
            }
        }
    }
    GrayBitmap {
        left: bitmap.left - radius,
        top: bitmap.top + radius,
        width: new_width,
        height: new_height,
        pixels,
    }
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
        let Some(page_digest) = document.page_digest(page) else {
            return Err(EngineError::new("page index out of range"));
        };
        if let Some(cached) = self.page_cache.borrow().get(&page_digest) {
            let mut cached = cached.clone();
            cached.index = page;
            return Ok(cached);
        }
        let page_data = document
            .pages
            .get(page)
            .ok_or_else(|| EngineError::new("page index out of range"))?;
        #[cfg(test)]
        self.render_misses.set(self.render_misses.get() + 1);
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
                xdv::XdvElement::Glyphs {
                    font_id,
                    color_rgba,
                    glyphs,
                } => {
                    let Some(font) = document.fonts.get(font_id) else {
                        continue;
                    };
                    let size_px = (font.size_pt * scale).round().max(1.0) as u32;
                    // Element color (special-driven) wins over the font's own
                    // XDV color; both default to black.
                    let rgba = color_rgba.or(font.color_rgba).unwrap_or(0x000000ff) | 0xff;
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
                xdv::XdvElement::Image {
                    x_pt,
                    y_pt,
                    w_pt,
                    h_pt,
                    scale: image_scale,
                    path,
                } => {
                    let Some(image) = self.decode_image(path) else {
                        continue;
                    };
                    // The bbox form carries the display size directly; the
                    // matrix form sizes from the decoded native pixels
                    // (1px = 1bp) times the special's scale factor.
                    let (display_w_pt, display_h_pt) = match image_scale {
                        Some(factor) => (image.width as f64 * factor, image.height as f64 * factor),
                        None => (*w_pt, *h_pt),
                    };
                    let dest_w = (display_w_pt * scale).round().max(1.0) as i64;
                    let dest_h = (display_h_pt * scale).round().max(1.0) as i64;
                    let dest_x = (origin_px + x_pt * scale).floor() as i64;
                    let dest_y = (origin_px + y_pt * scale).floor() as i64;
                    blit_image(
                        &mut canvas,
                        width,
                        height,
                        &image,
                        dest_x,
                        dest_y,
                        dest_w,
                        dest_h,
                    );
                }
            }
        }

        let rendered = RenderedPage {
            index: page,
            width,
            height,
            pixels_rgba: canvas,
        };
        {
            let mut cache = self.page_cache.borrow_mut();
            if cache.len() >= PAGE_CACHE_CAPACITY {
                if let Some(oldest) = cache.keys().next().copied() {
                    cache.remove(&oldest);
                }
            }
            cache.insert(page_digest, rendered.clone());
        }
        Ok(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xdv;

    struct SystemFont;
    impl FontResolver for SystemFont {
        fn find_font_file(&mut self, name: &str, extensions: &[&str]) -> Option<Vec<u8>> {
            if !name.eq_ignore_ascii_case("times") {
                return None;
            }
            extensions
                .iter()
                .map(|extension| format!("{name}.{extension}"))
                .filter_map(|candidate| {
                    let path = std::path::PathBuf::from(r"C:\Windows\Fonts").join(candidate);
                    std::fs::read(path).ok()
                })
                .next()
        }
    }

    /// Builds a two-page XDV where `page1_code` controls the second page's
    /// only glyph, so page 0 stays byte-identical while page 1 differs.
    fn two_page_xdv(page1_code: u16) -> Vec<u8> {
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&[247u8, 7]); // PRE, XDV id 7
        stream.extend_from_slice(&25_400_000u32.to_be_bytes());
        stream.extend_from_slice(&473_628_672u32.to_be_bytes());
        stream.extend_from_slice(&1000u32.to_be_bytes());
        stream.push(0);
        for (page_index, code) in [65u16, page1_code].into_iter().enumerate() {
            stream.extend_from_slice(&[139u8]); // BOP
            stream.extend_from_slice(&(page_index as i32).to_be_bytes());
            stream.extend_from_slice(&[0u8; 36]);
            stream.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
            stream.extend_from_slice(&[252u8]); // DEFINE_NATIVE_FONT
            stream.extend_from_slice(&40u32.to_be_bytes());
            stream.extend_from_slice(&(12u32 << 16).to_be_bytes());
            stream.extend_from_slice(&0u16.to_be_bytes());
            stream.push(b"times".len() as u8);
            stream.extend_from_slice(b"times");
            stream.extend_from_slice(&0u32.to_be_bytes());
            stream.extend_from_slice(&[236u8]); // FNT2
            stream.extend_from_slice(&40u16.to_be_bytes());
            stream.extend_from_slice(&[253u8]); // SET_GLYPHS
            stream.extend_from_slice(&100i32.to_be_bytes());
            stream.extend_from_slice(&1u16.to_be_bytes());
            stream.extend_from_slice(&0i32.to_be_bytes());
            stream.extend_from_slice(&0i32.to_be_bytes());
            stream.extend_from_slice(&code.to_be_bytes());
            stream.extend_from_slice(&[140u8]); // EOP
        }
        stream.extend_from_slice(&[248u8]); // POST
        stream.extend_from_slice(&[0u8; 24]);
        stream.extend_from_slice(&2u16.to_be_bytes());
        stream.extend_from_slice(&1u16.to_be_bytes());
        stream.extend_from_slice(&[249u8]); // POST_POST
        stream.extend_from_slice(&0u32.to_be_bytes());
        stream.push(7);
        stream.extend_from_slice(&[223u8; 8]);
        stream
    }

    #[cfg(windows)]
    #[test]
    fn page_cache_reuses_unchanged_pages_across_artifacts() {
        if !std::path::Path::new(r"C:\Windows\Fonts\times.ttf").is_file() {
            return;
        }
        let backend = XdvGlyphRenderBackend::new(Box::new(SystemFont));
        let first = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: two_page_xdv(66),
            source_name: Some("first.xdv".to_string()),
        };
        assert!(backend.render_page(&first, 0).is_ok());
        assert!(backend.render_page(&first, 1).is_ok());
        assert_eq!(backend.render_misses.get(), 2);

        // Rebuild: page 0 is unchanged, page 1 changed.
        let second = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: two_page_xdv(67),
            source_name: Some("second.xdv".to_string()),
        };
        let reused = backend.render_page(&second, 0).unwrap();
        assert_eq!(
            backend.render_misses.get(),
            2,
            "unchanged page must be reused"
        );
        assert_eq!(reused.index, 0);
        let fresh = backend.render_page(&second, 1).unwrap();
        assert_eq!(
            backend.render_misses.get(),
            3,
            "changed page must re-render"
        );
        assert_eq!(fresh.index, 1);
    }

    struct NullFonts;
    impl FontResolver for NullFonts {
        fn find_font_file(&mut self, _name: &str, _extensions: &[&str]) -> Option<Vec<u8>> {
            None
        }
    }

    struct StubImages {
        bytes: Vec<u8>,
    }
    impl ImageLoader for StubImages {
        fn find_image_file(&mut self, _path: &str) -> Option<Vec<u8>> {
            Some(self.bytes.clone())
        }
    }

    /// Encodes a `width × height` PNG of a solid color through the png crate.
    fn solid_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            let pixels = vec![rgba; (width * height) as usize]
                .into_iter()
                .flatten()
                .collect::<Vec<u8>>();
            writer.write_image_data(&pixels).unwrap();
        }
        bytes
    }

    /// Builds a one-page XDV with a pagesize special and one bbox-form
    /// `pdf:image` special placed at (10pt, 10pt).
    fn image_xdv(image_path: &str) -> Vec<u8> {
        let pagesize = b"pdf:pagesize width 614.295pt height 794.96999pt";
        let special = format!("pdf:image bbox 0 0 4 4 clip 0 width 40pt ({} )", image_path);
        let special = special.replace(" )", ")");
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&[247u8, 7]); // PRE, XDV id 7
        stream.extend_from_slice(&25_400_000u32.to_be_bytes());
        stream.extend_from_slice(&473_628_672u32.to_be_bytes());
        stream.extend_from_slice(&1000u32.to_be_bytes());
        stream.push(0);
        stream.extend_from_slice(&[139u8]); // BOP
        stream.extend_from_slice(&[0u8; 40]);
        stream.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        stream.extend_from_slice(&[239u8, pagesize.len() as u8]);
        stream.extend_from_slice(pagesize);
        stream.extend_from_slice(&[239u8, special.len() as u8]);
        stream.extend_from_slice(special.as_bytes());
        stream.extend_from_slice(&[140u8]); // EOP
        stream.extend_from_slice(&[248u8]); // POST
        stream.extend_from_slice(&[0u8; 24]);
        stream.extend_from_slice(&1u16.to_be_bytes());
        stream.extend_from_slice(&1u16.to_be_bytes());
        stream.extend_from_slice(&[249u8]); // POST_POST
        stream.extend_from_slice(&0u32.to_be_bytes());
        stream.push(7);
        stream.extend_from_slice(&[223u8; 8]);
        stream
    }

    #[test]
    fn pdf_image_specials_composite_onto_rendered_page() {
        let png = solid_png(2, 2, [255, 0, 0, 255]);
        let backend = XdvGlyphRenderBackend::with_image_loader(
            Box::new(NullFonts),
            Box::new(StubImages { bytes: png }),
        );
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: image_xdv("logo.png"),
            source_name: Some("images.xdv".to_string()),
        };
        let page = backend.render_page(&artifact, 0).unwrap();
        // The image is 40×40pt at (10,10) → 53×53 px at 96dpi; the page is
        // white everywhere else. Verify image pixels are red and pixels
        // outside the image area stay white.
        let red_at = |px: usize, py: usize| -> [u8; 4] {
            let offset = (py * page.width as usize + px) * 4;
            [
                page.pixels_rgba[offset],
                page.pixels_rgba[offset + 1],
                page.pixels_rgba[offset + 2],
                page.pixels_rgba[offset + 3],
            ]
        };
        // px_per_pt = 4/3: 10pt → 13.33 → origin 96px + 13 = 109.
        let inside = red_at(120, 120);
        assert_eq!(inside, [255, 0, 0, 255], "image area must be red");
        let outside = red_at(5, 5);
        assert_eq!(outside, [255, 255, 255, 255], "corner stays white");
        // Second render must come from the page cache.
        assert!(backend.render_page(&artifact, 0).is_ok());
        assert_eq!(backend.render_misses.get(), 1);
    }

    #[test]
    fn pdf_image_specials_render_placeholder_when_loader_missing() {
        let backend = XdvGlyphRenderBackend::with_image_loader(
            Box::new(NullFonts),
            Box::new(NullImageLoader),
        );
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: image_xdv("missing.png"),
            source_name: Some("missing-images.xdv".to_string()),
        };
        // Missing images render without failing the whole page.
        let page = backend.render_page(&artifact, 0).unwrap();
        assert!(
            page.pixels_rgba
                .chunks_exact(4)
                .all(|pixel| pixel == [255, 255, 255, 255])
        );
    }
}
