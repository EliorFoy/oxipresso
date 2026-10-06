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
pub trait ImageLoader: Send {
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

/// Parses an Adobe Font Metrics file's `StartCharMetrics` table into a
/// charcode -> glyph-name map: lines like
/// `C 25 ; WX 500 ; N pi ; B 20 -13 489 621 ;`.
pub fn parse_afm_charmetrics(bytes: &[u8]) -> Option<HashMap<u32, String>> {
    let text = String::from_utf8_lossy(bytes);
    let mut map = HashMap::new();
    let mut in_metrics = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("StartCharMetrics") {
            in_metrics = true;
            continue;
        }
        if line.starts_with("EndCharMetrics") {
            break;
        }
        if !in_metrics || !line.starts_with("C ") {
            continue;
        }
        let code: u32 = line[2..].split(';').next()?.trim().parse().ok()?;
        let name = line.split("; N ").nth(1)?.split(';').next()?.trim();
        if !name.is_empty() {
            map.insert(code, name.to_string());
        }
    }
    (!map.is_empty()).then_some(map)
}

pub struct XdvGlyphRenderBackend {
    /// Page rasterization scale in pixels per point (96 dpi = 4/3).
    pub px_per_pt: f64,
    resolver: RefCell<Box<dyn FontResolver>>,
    image_loader: RefCell<Box<dyn ImageLoader>>,
    library: RefCell<Option<ft::FT_Library>>,
    faces: RefCell<HashMap<(String, u32), ft::FT_Face>>,
    font_files: RefCell<HashMap<String, Option<Rc<Vec<u8>>>>>,
    /// Classic Type1 fonts: charcode -> glyph-name table parsed from the
    /// font's AFM (the encoding vector), keyed by font name.
    afm_tables: RefCell<HashMap<String, Option<Rc<HashMap<u32, String>>>>>,
    glyph_cache: RefCell<HashMap<(String, u32, u32, u32, u64), Option<Rc<GrayBitmap>>>>,
    /// Decoded images keyed by (path, content hash) so an in-place image edit
    /// (same path, different bytes) re-decodes rather than serving a stale one.
    image_cache: RefCell<HashMap<(String, u64), Option<Rc<DecodedImage>>>>,
    parsed: RefCell<Option<(u64, Rc<xdv::XdvDocument>)>>,
    /// Rendered pages keyed by XDV page content digest. Pages whose digest
    /// matches a previous rebuild are reused without re-rendering, which is
    /// the renderer-side half of the incremental rebuild model.
    page_cache: RefCell<HashMap<u64, RenderedPage>>,
    /// Page background (canvas fill) and default ink color, RGB, driven by the
    /// editor's `(theme bg fg)` command (TeXpresso paints the canvas with
    /// `background_color` and uses `foreground_color` as the default ink). The
    /// defaults reproduce the previous hardcoded white-background / black-ink
    /// rendering; `set_theme` changes them and clears the page cache.
    background: std::cell::Cell<[u8; 3]>,
    foreground: std::cell::Cell<[u8; 3]>,
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
            afm_tables: RefCell::new(HashMap::new()),
            glyph_cache: RefCell::new(HashMap::new()),
            image_cache: RefCell::new(HashMap::new()),
            parsed: RefCell::new(None),
            page_cache: RefCell::new(HashMap::new()),
            background: std::cell::Cell::new([255, 255, 255]),
            foreground: std::cell::Cell::new([0, 0, 0]),
            #[cfg(test)]
            render_misses: std::cell::Cell::new(0),
        }
    }

    /// Set the page background and default ink colors (editor `(theme bg fg)`),
    /// clearing the page cache so already-rendered pages are repainted.
    pub fn set_theme(&self, background: [u8; 3], foreground: [u8; 3]) {
        if self.background.get() == background && self.foreground.get() == foreground {
            return;
        }
        self.background.set(background);
        self.foreground.set(foreground);
        self.page_cache.borrow_mut().clear();
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

    /// The classic font's charcode -> glyph-name table from its AFM file
    /// (the font's encoding vector), resolved once per font name.
    fn afm_table(&self, name: &str) -> Option<Rc<HashMap<u32, String>>> {
        if let Some(cached) = self.afm_tables.borrow().get(name) {
            return cached.clone();
        }
        let table = self
            .resolver
            .borrow_mut()
            .find_font_file(name, &["afm"])
            .and_then(|bytes| parse_afm_charmetrics(&bytes))
            .map(Rc::new);
        self.afm_tables
            .borrow_mut()
            .insert(name.to_string(), table.clone());
        table
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
            // Native XDV fonts: `code` is a GLYPH ID (harfbuzz shaping output)
            // and must be loaded by index. Classic TFM/Type1 fonts: `code` is
            // a CHAR CODE in the font's TeX encoding — the Type1 face only
            // exposes a synthesized UNICODE charmap (math codes have none), so
            // map charcode -> glyph NAME via the font's AFM, then name -> gid
            // through FT_Get_Name_Index.
            let glyph_index = if font.native {
                code
            } else {
                let afm = self.afm_table(&font.name);
                let via_afm = afm
                    .as_ref()
                    .and_then(|table| table.get(&code))
                    .and_then(|name| {
                        let cname = std::ffi::CString::new(name.as_str()).ok()?;
                        let gid = ft::FT_Get_Name_Index(face, cname.as_ptr());
                        (gid != 0).then_some(gid)
                    });
                match via_afm {
                    Some(gid) => gid,
                    None => {
                        let mapped = ft::FT_Get_Char_Index(face, code);
                        if mapped == 0 { code } else { mapped }
                    }
                }
            };
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
    /// Reads every referenced image's current bytes once (via the loader),
    /// returning `path -> (content_hash, decoded)`. Decodes are memoized in
    /// `image_cache` by `(path, content_hash)`, so an in-place file edit
    /// (new bytes, same path) yields a new hash and forces re-decode. The
    /// content hashes let `render_page` fold image state into the page-cache
    /// key, so edited images invalidate the cached rendered page.
    fn load_page_images(
        &self,
        page_data: &xdv::XdvPage,
    ) -> HashMap<String, (u64, Option<Rc<DecodedImage>>)> {
        let mut result: HashMap<String, (u64, Option<Rc<DecodedImage>>)> = HashMap::new();
        for element in &page_data.elements {
            let xdv::XdvElement::Image { path, .. } = element else {
                continue;
            };
            if result.contains_key(path) {
                continue;
            }
            let bytes = self.image_loader.borrow_mut().find_image_file(path);
            let Some(bytes) = bytes else {
                result.insert(path.clone(), (0, None));
                continue;
            };
            let hash = fnv_hash(&bytes);
            let cache_key = (path.clone(), hash);
            // Clone the cache hit first so the immutable borrow ends before
            // the mutable borrow below (a `match` scrutinee borrow would live
            // through the arm and panic on `borrow_mut`).
            let cached = self.image_cache.borrow().get(&cache_key).cloned();
            let decoded = match cached {
                Some(decoded) => decoded,
                None => {
                    let decoded = decode_png(&bytes);
                    self.image_cache
                        .borrow_mut()
                        .insert(cache_key, decoded.clone());
                    decoded
                }
            };
            result.insert(path.clone(), (hash, decoded));
        }
        result
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
/// `dest_w × dest_h` pixels at `dest_x/dest_y`. Each destination pixel is the
/// box-average of the source rectangle it covers: identical to nearest-
/// neighbour for 1:1/upscale (a single source pixel) but it blends the covered
/// pixels when downscaling instead of dropping most of the image.
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
    let iw = image.width as i64;
    let ih = image.height as i64;
    for row in 0..dest_h {
        let page_y = dest_y + row;
        if page_y < 0 || page_y >= canvas_height as i64 {
            continue;
        }
        let y0 = (row * ih / dest_h).clamp(0, ih - 1);
        let y1 = (((row + 1) * ih / dest_h).max(y0 + 1)).min(ih);
        for column in 0..dest_w {
            let page_x = dest_x + column;
            if page_x < 0 || page_x >= canvas_width as i64 {
                continue;
            }
            let x0 = (column * iw / dest_w).clamp(0, iw - 1);
            let x1 = (((column + 1) * iw / dest_w).max(x0 + 1)).min(iw);
            // Average in PREMULTIPLIED space: transparent pixels must not drag
            // the color toward black. `sum_pa` accumulates coverage; the shown
            // colour is the alpha-weighted mean, and the region's coverage alpha
            // is the mean alpha. For fully-opaque sources this reduces to the
            // straight mean, so solid images are unchanged.
            let (mut sum_pa, mut sum_pr, mut sum_pg, mut sum_pb) = (0u64, 0u64, 0u64, 0u64);
            let mut count = 0u64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let s = (sy * iw + sx) as usize * 4;
                    let a = image.pixels[s + 3] as u64;
                    sum_pa += a;
                    sum_pr += image.pixels[s] as u64 * a;
                    sum_pg += image.pixels[s + 1] as u64 * a;
                    sum_pb += image.pixels[s + 2] as u64 * a;
                    count += 1;
                }
            }
            if sum_pa == 0 {
                continue; // fully transparent rectangle: leave the page as-is
            }
            let r = (sum_pr / sum_pa) as u32;
            let g = (sum_pg / sum_pa) as u32;
            let b = (sum_pb / sum_pa) as u32;
            let a = (sum_pa / count).min(255) as u32;
            let dest = (page_y as usize * canvas_width as usize + page_x as usize) * 4;
            if a >= 255 {
                canvas[dest] = r as u8;
                canvas[dest + 1] = g as u8;
                canvas[dest + 2] = b as u8;
                canvas[dest + 3] = 255;
            } else if a > 0 {
                // Source-over blend against the opaque white page.
                let inverse = 255 - a;
                canvas[dest] = ((r * a + canvas[dest] as u32 * inverse) / 255) as u8;
                canvas[dest + 1] = ((g * a + canvas[dest + 1] as u32 * inverse) / 255) as u8;
                canvas[dest + 2] = ((b * a + canvas[dest + 2] as u32 * inverse) / 255) as u8;
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
    // x' = x + slant*y, y growing upward from the baseline (bottom row). The
    // extreme shift is at the top row; the baseline row has shift 0. Allocate a
    // canvas spanning [min_shift, max_shift] so neither the right overhang of a
    // positive slant nor the left overhang of a negative slant is clipped.
    let extreme = slant * (bitmap.height as f64 - 1.0);
    let (shift_min, shift_max) = if extreme >= 0.0 {
        (0i64, extreme.ceil() as i64)
    } else {
        (extreme.floor() as i64, 0i64)
    };
    let pad_left = -shift_min; // room on the left for negative slant
    let new_width = (bitmap.width as i64 + (shift_max - shift_min)) as u32;
    let mut pixels = vec![0u8; (new_width as usize) * (bitmap.height as usize)];
    for row_from_top in 0..bitmap.height {
        let baseline_distance = (bitmap.height - 1 - row_from_top) as f64;
        let shift = (slant * baseline_distance).round() as i64;
        let dest_base = shift + pad_left; // in [0, shift_max - shift_min]
        for col in 0..bitmap.width {
            let source = bitmap.pixels[(row_from_top * bitmap.width + col) as usize];
            if source == 0 {
                continue;
            }
            let dest = dest_base + col as i64;
            if dest >= 0 && (dest as u32) < new_width {
                let index = row_from_top as usize * new_width as usize + dest as usize;
                pixels[index] = source;
            }
        }
    }
    GrayBitmap {
        // Baseline (shift 0) content sits at dest_base = pad_left; offset the
        // bearing by -pad_left so the glyph's baseline stays at its original x.
        left: bitmap.left - pad_left,
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
        self.render_page_inner(artifact, page, 1.0)
    }

    fn render_page_scaled(
        &self,
        artifact: &DocumentArtifact,
        page: usize,
        extra_scale: f32,
    ) -> Result<RenderedPage> {
        self.render_page_inner(artifact, page, extra_scale)
    }
}

impl XdvGlyphRenderBackend {
    fn render_page_inner(
        &self,
        artifact: &DocumentArtifact,
        page: usize,
        extra_scale: f32,
    ) -> Result<RenderedPage> {
        if !matches!(artifact.kind, ArtifactKind::Xdv | ArtifactKind::Dvi) {
            return Err(EngineError::new(
                "XdvGlyphRenderBackend only accepts XDV/DVI artifacts",
            ));
        }
        let document = self.parse_document(artifact)?;
        let Some(page_digest) = document.page_digest(page) else {
            return Err(EngineError::new("page index out of range"));
        };
        let page_data = document
            .pages
            .get(page)
            .ok_or_else(|| EngineError::new("page index out of range"))?;
        // Load referenced images once (also detects in-place content edits) and
        // fold their content hashes into the cache key, so replacing an image
        // file invalidates the cached rendered page even though the XDV bytes
        // (which only reference path + geometry) are unchanged.
        let images = self.load_page_images(page_data);
        let image_salt = images
            .values()
            .fold(0u64, |acc, (hash, _)| acc ^ hash.rotate_left(17));
        // Different zoom scales must not share a cache entry: fold the scale
        // bits into the key alongside the image salt.
        let scale_salt = (extra_scale as f64).to_bits();
        let cache_key = page_digest
            ^ image_salt.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ scale_salt.wrapping_mul(0xA24B_A5D3_1C0E_4D5F);
        if let Some(cached) = self.page_cache.borrow().get(&cache_key) {
            let mut cached = cached.clone();
            cached.index = page;
            return Ok(cached);
        }
        #[cfg(test)]
        self.render_misses.set(self.render_misses.get() + 1);
        let scale = self.px_per_pt * f64::from(extra_scale);
        let width = (page_data.width_pt * scale).round().max(1.0) as u32;
        let height = (page_data.height_pt * scale).round().max(1.0) as u32;
        // A corrupt/hand-edited .xdv (the viewer and `--features freetype` builds
        // open artifacts from disk) can declare an absurd pdf:pagesize; the `as
        // u32` cast saturates to u32::MAX, and width*height*4 then overflows u64
        // (debug panic) or wraps into an undersized buffer that the glyph blit
        // would run off. Cap to a bound far above any real TeX page (maxdimen is
        // ~21888px at 96dpi) and use checked arithmetic — a bogus page is an Err,
        // never a panic. Real documents are unaffected.
        const MAX_PAGE_PX: u32 = 65_536;
        if width > MAX_PAGE_PX || height > MAX_PAGE_PX {
            return Err(EngineError::new("XDV page dimensions out of range"));
        }
        let pixel_count = (width as usize)
            .checked_mul(height as usize)
            .and_then(|area| area.checked_mul(4))
            .ok_or_else(|| EngineError::new("XDV page size overflow"))?;
        // Editor theme colors (default white background / black ink): the page
        // is painted with the background and uncolored glyphs/rules use the
        // foreground as their ink, matching TeXpresso's background_color /
        // foreground_color renderer config.
        let [bg_r, bg_g, bg_b] = self.background.get();
        let [fg_r, fg_g, fg_b] = self.foreground.get();
        let foreground_packed = ((fg_r as u32) << 16) | ((fg_g as u32) << 8) | (fg_b as u32);
        let mut canvas = vec![0u8; pixel_count];
        for pixel in canvas.chunks_exact_mut(4) {
            pixel[0] = bg_r;
            pixel[1] = bg_g;
            pixel[2] = bg_b;
            pixel[3] = 255;
        }

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
                            canvas[offset] = fg_r;
                            canvas[offset + 1] = fg_g;
                            canvas[offset + 2] = fg_b;
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
                    // XDV color; both default to the theme foreground (ink).
                    let rgba = color_rgba.or(font.color_rgba).unwrap_or(foreground_packed) | 0xff;
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
                    let Some(image) = images
                        .get(path.as_str())
                        .and_then(|(_, decoded)| decoded.clone())
                    else {
                        continue;
                    };
                    // The bbox form carries the display size directly; the
                    // matrix form sizes from the decoded native pixels
                    // (1px = 1bp) times the special's per-axis (x, y) scale.
                    let (display_w_pt, display_h_pt) = match image_scale {
                        Some([sx, sy]) => (image.width as f64 * *sx, image.height as f64 * *sy),
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
            cache.insert(cache_key, rendered.clone());
        }
        Ok(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Builds a one-page XDV with a pagesize special and a matrix-form
    /// `pdf:image` special (`\XeTeXpicfile`), which carries only a scale, not
    /// an explicit display size.
    fn image_xdv_matrix(special: &str) -> Vec<u8> {
        let pagesize = b"pdf:pagesize width 614.295pt height 794.96999pt";
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
    fn pdf_image_matrix_form_sizes_from_native_pixels_times_scale() {
        // 24×24 native PNG; matrix scale 2.5 => 60pt display; at 96 dpi
        // (px_per_pt 4/3) => 80px, placed at the DVI origin (96px, 96px).
        let png = solid_png(24, 24, [0, 0, 255, 255]);
        let backend = XdvGlyphRenderBackend::with_image_loader(
            Box::new(NullFonts),
            Box::new(StubImages { bytes: png }),
        );
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: image_xdv_matrix("pdf:image matrix 2.5 0.0 0.0 2.5 0.0 0.0 page 0 (logo.png)"),
            source_name: Some("matrix.xdv".to_string()),
        };
        let page = backend.render_page(&artifact, 0).unwrap();
        let at = |px: usize, py: usize| -> [u8; 4] {
            let offset = (py * page.width as usize + px) * 4;
            [
                page.pixels_rgba[offset],
                page.pixels_rgba[offset + 1],
                page.pixels_rgba[offset + 2],
                page.pixels_rgba[offset + 3],
            ]
        };
        // Origin at 96px; the image spans [96, 176). Center and an in-range
        // edge are blue; just past the far edge is white — this asserts the
        // 60pt→80px sizing (bbox form would not exercise the scale×native path).
        assert_eq!(at(136, 136), [0, 0, 255, 255], "image center is blue");
        assert_eq!(at(170, 170), [0, 0, 255, 255], "near-edge still blue");
        assert_eq!(
            at(190, 190),
            [255, 255, 255, 255],
            "past the far edge white"
        );
        assert_eq!(at(5, 5), [255, 255, 255, 255], "corner stays white");
    }

    /// One-page XDV with a `set_rule` (50pt square at the origin pen), used to
    /// exercise theme background (untouched corner) and foreground (the rule).
    fn rule_xdv() -> Vec<u8> {
        xdv_stream(b"pdf:pagesize width 614.295pt height 794.96999pt")
    }

    fn xdv_stream(pagesize: &[u8]) -> Vec<u8> {
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
        stream.extend_from_slice(&[132u8]); // SET_RULE height width
        stream.extend_from_slice(&(50 * 65536i32).to_be_bytes()); // height 50pt
        stream.extend_from_slice(&(50 * 65536i32).to_be_bytes()); // width 50pt
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
    fn absurd_xdv_page_size_is_rejected_without_overflow_panic() {
        // A corrupt/hand-edited .xdv declaring a huge pdf:pagesize must not
        // overflow the `width*height*4` byte count (debug panic / release
        // undersized buffer). It should be a graceful Err.
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: xdv_stream(b"pdf:pagesize width 100000000pt height 100000000pt"),
            source_name: Some("huge.xdv".to_string()),
        };
        let backend = XdvGlyphRenderBackend::new(Box::new(NullFonts));
        assert!(
            backend.render_page(&artifact, 0).is_err(),
            "absurd page dimensions must be an Err, not a panic"
        );
    }

    #[test]
    fn theme_colors_paint_background_and_ink() {
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: rule_xdv(),
            source_name: Some("theme.xdv".to_string()),
        };
        let backend = XdvGlyphRenderBackend::new(Box::new(NullFonts));
        let at = |page: &RenderedPage, px: usize, py: usize| -> [u8; 4] {
            let o = (py * page.width as usize + px) * 4;
            [
                page.pixels_rgba[o],
                page.pixels_rgba[o + 1],
                page.pixels_rgba[o + 2],
                page.pixels_rgba[o + 3],
            ]
        };
        // Default theme: white background, black ink (unchanged legacy look).
        let page = backend.render_page(&artifact, 0).unwrap();
        assert_eq!(at(&page, 5, 5), [255, 255, 255, 255], "default bg white");
        assert_eq!(
            at(&page, 120, 120),
            [0, 0, 0, 255],
            "default ink black rule"
        );

        // Editor theme: blue background, red ink. set_theme must repaint cached
        // pages, so the second render reflects it without a new backend.
        backend.set_theme([10, 20, 30], [200, 10, 10]);
        let page = backend.render_page(&artifact, 0).unwrap();
        assert_eq!(
            at(&page, 5, 5),
            [10, 20, 30, 255],
            "background painted with theme bg"
        );
        assert_eq!(
            at(&page, 120, 120),
            [200, 10, 10, 255],
            "rule (default ink) painted with theme fg"
        );
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

    /// Image loader whose current bytes can be swapped between renders
    /// (shared via `Rc<RefCell<..>>`) to simulate an in-place file edit.
    struct SwapImages {
        current: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl ImageLoader for SwapImages {
        fn find_image_file(&mut self, _path: &str) -> Option<Vec<u8>> {
            Some(self.current.lock().unwrap().clone())
        }
    }

    #[test]
    fn in_place_image_edit_invalidates_cached_page() {
        // Same XDV (image referenced by an unchanged path/geometry), but the
        // image file's bytes change between renders. The renderer must NOT
        // serve the stale cached page — image content is folded into the
        // page-cache key. Guards the documented image-edit-invalidation fix.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(solid_png(2, 2, [255, 0, 0, 255])));
        let backend = XdvGlyphRenderBackend::with_image_loader(
            Box::new(NullFonts),
            Box::new(SwapImages {
                current: std::sync::Arc::clone(&shared),
            }),
        );
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Xdv,
            bytes: image_xdv("logo.png"),
            source_name: Some("edit.xdv".to_string()),
        };
        let rgb_at = |page: &RenderedPage, x: usize, y: usize| {
            let o = (y * page.width as usize + x) * 4;
            [
                page.pixels_rgba[o],
                page.pixels_rgba[o + 1],
                page.pixels_rgba[o + 2],
            ]
        };
        let first = backend.render_page(&artifact, 0).unwrap();
        assert_eq!(rgb_at(&first, 120, 120), [255, 0, 0], "first render: red");

        // Replace the image bytes in place (same path); the XDV is unchanged.
        *shared.lock().unwrap() = solid_png(2, 2, [0, 0, 255, 255]);
        let second = backend.render_page(&artifact, 0).unwrap();
        assert_eq!(
            rgb_at(&second, 120, 120),
            [0, 0, 255],
            "in-place image edit must invalidate the cached page and show the new image"
        );
    }

    #[test]
    fn blit_image_box_averages_downscale() {
        // A 2x2 checkerboard (red/blue diagonal) scaled to 1x1 must average to
        // ~(127,0,127), not collapse to a single corner pixel as nearest would.
        let checker = DecodedImage {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 0, 255, // 0,0 red
                0, 0, 255, 255, // 1,0 blue
                0, 0, 255, 255, // 0,1 blue
                255, 0, 0, 255, // 1,1 red
            ],
        };
        let mut canvas = vec![0u8; 4]; // 1x1
        blit_image(&mut canvas, 1, 1, &checker, 0, 0, 1, 1);
        assert_eq!(
            [canvas[0], canvas[1], canvas[2]],
            [127, 0, 127],
            "downscale must box-average, got {:?}",
            &canvas[0..3]
        );
        assert_eq!(canvas[3], 255);
    }

    #[test]
    fn blit_image_upscale_fills_uniform_source() {
        // 1x1 opaque red stretched to 2x2 fills every destination pixel red.
        let red = DecodedImage {
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        };
        let mut canvas = vec![255u8; 4 * 4]; // 2x2 white
        blit_image(&mut canvas, 2, 2, &red, 0, 0, 2, 2);
        for px in canvas.chunks_exact(4) {
            assert_eq!(px, [255, 0, 0, 255], "upscale fills red");
        }
    }

    #[test]
    fn blit_image_downscale_uses_premultiplied_alpha() {
        // 2x1: opaque red next to a fully-transparent (black) pixel, scaled to
        // 1x1. Straight averaging would pull the red channel down toward the
        // transparent pixel's stored black (R~191); premultiplied averaging
        // keeps R at 255 and only halves coverage, so the result stays pure red,
        // just 50% blended over the white page: (255,128,128).
        let image = DecodedImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 0, 0],
        };
        let mut canvas = vec![255u8; 4]; // 1x1 white page
        blit_image(&mut canvas, 1, 1, &image, 0, 0, 1, 1);
        assert_eq!(
            [canvas[0], canvas[1], canvas[2]],
            [255, 128, 128],
            "transparent pixel must not darken the color (premultiplied downscale)"
        );
    }

    #[test]
    fn extend_widens_and_scales_left() {
        let bm = GrayBitmap {
            left: 3,
            top: 7,
            width: 2,
            height: 1,
            pixels: vec![200, 100],
        };
        let out = apply_extend(bm, 2.0);
        assert_eq!(out.width, 4);
        assert_eq!(out.pixels, vec![200, 200, 100, 100]);
        assert_eq!(out.left, 6); // left bearing scaled by the extend factor
        assert_eq!(out.top, 7);
    }

    #[test]
    fn embolden_dilates_to_square_footprint() {
        // strength 0.125 => radius round(0.125*8) = 1 => 1px grows to a 3x3 block.
        let bm = GrayBitmap {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
            pixels: vec![255],
        };
        let out = apply_embolden(bm, 0.125);
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 3);
        assert!(out.pixels.iter().all(|&v| v == 255));
        assert_eq!(out.left, -1);
        assert_eq!(out.top, 1);
    }

    #[test]
    fn slant_shears_right_without_dropping_ink() {
        // A solid 2x3 glyph sheared with slant 1.0: the top rows shift right.
        // A correct shear preserves all ink (a wider canvas accommodates it).
        let bm = GrayBitmap {
            left: 0,
            top: 0,
            width: 2,
            height: 3,
            pixels: vec![200, 200, 200, 200, 200, 200],
        };
        let out = apply_slant(bm, 1.0);
        let sum: u32 = out.pixels.iter().map(|&v| v as u32).sum();
        assert_eq!(
            sum,
            6 * 200,
            "positive slant must not clip away slanted-overhang pixels (got width {})",
            out.width
        );
    }

    #[test]
    fn slant_negative_preserves_ink() {
        let bm = GrayBitmap {
            left: 0,
            top: 0,
            width: 2,
            height: 3,
            pixels: vec![200, 200, 200, 200, 200, 200],
        };
        let out = apply_slant(bm, -1.0);
        let sum: u32 = out.pixels.iter().map(|&v| v as u32).sum();
        assert_eq!(
            sum,
            6 * 200,
            "negative slant must not clip the left overhang"
        );
        assert_eq!(out.left, -2); // canvas grows 2px to the left, baseline kept at original x
    }

    #[test]
    fn blit_gray_composites_coverage_and_clips() {
        // Black glyph (0x000000FF) over an opaque-white page: full coverage ->
        // black; 50% coverage -> midpoint gray; result alpha forced opaque.
        let bm = GrayBitmap {
            left: 0,
            top: 0,
            width: 2,
            height: 1,
            pixels: vec![255, 128],
        };
        let mut canvas = vec![255u8; 2 * 4]; // 2x1 white RGBA
        blit_gray(&mut canvas, 2, 1, &bm, 0, 0, 0x0000_00FF);
        assert_eq!(&canvas[0..4], &[0, 0, 0, 255], "full coverage -> black");
        assert_eq!(
            &canvas[4..8],
            &[127, 127, 127, 255],
            "50% coverage of black over white -> 127 gray, opaque"
        );

        // A glyph placed off the left edge must not panic and uses the correct
        // (in-bounds) source column: left=-1 means column 1 lands at x=0.
        let mut clipped = vec![255u8; 4]; // 1x1 white
        blit_gray(
            &mut clipped,
            1,
            1,
            &GrayBitmap {
                left: 0,
                top: 0,
                width: 2,
                height: 1,
                pixels: vec![0, 255],
            },
            -1,
            0,
            0x0000_00FF,
        );
        assert_eq!(
            &clipped[..],
            &[0, 0, 0, 255],
            "clipped glyph uses column at x=0"
        );
    }
}
