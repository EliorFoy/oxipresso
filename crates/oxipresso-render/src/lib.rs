use oxipresso_engine_api::{ArtifactKind, DocumentArtifact, EngineError, Result};
use std::collections::HashMap;
#[cfg(feature = "pdfium")]
use std::{cell::RefCell, fmt};

#[cfg(feature = "pdfium")]
use pdfium_render::prelude::*;

const PLACEHOLDER_WIDTH: u32 = 96;
const PLACEHOLDER_HEIGHT: u32 = 128;
const DVI_GLYPH_WIDTH: i64 = 500;
const DVI_GLYPH_HEIGHT: i64 = 700;
#[cfg(feature = "pdfium")]
const PDFIUM_TARGET_WIDTH: i32 = 1400;
#[cfg(feature = "pdfium")]
const PDFIUM_MAX_HEIGHT: i32 = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPage {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub pixels_rgba: Vec<u8>,
}

pub trait RenderBackend {
    fn page_count(&self, artifact: &DocumentArtifact) -> Result<usize>;
    fn render_page(&self, artifact: &DocumentArtifact, page: usize) -> Result<RenderedPage>;
}

pub struct AutoRenderBackend {
    metadata: PdfMetadataRenderBackend,
    #[cfg(feature = "pdfium")]
    pdfium: PdfiumRenderBackend,
}

impl Default for AutoRenderBackend {
    fn default() -> Self {
        Self {
            metadata: PdfMetadataRenderBackend,
            #[cfg(feature = "pdfium")]
            pdfium: PdfiumRenderBackend::default(),
        }
    }
}

impl std::fmt::Debug for AutoRenderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoRenderBackend").finish_non_exhaustive()
    }
}

impl RenderBackend for AutoRenderBackend {
    fn page_count(&self, artifact: &DocumentArtifact) -> Result<usize> {
        #[cfg(feature = "pdfium")]
        if artifact.kind == ArtifactKind::Pdf {
            if let Ok(count) = self.pdfium.page_count(artifact) {
                return Ok(count);
            }
        }
        self.metadata.page_count(artifact)
    }

    fn render_page(&self, artifact: &DocumentArtifact, page: usize) -> Result<RenderedPage> {
        #[cfg(feature = "pdfium")]
        if artifact.kind == ArtifactKind::Pdf {
            if let Ok(rendered) = self.pdfium.render_page(artifact, page) {
                return Ok(rendered);
            }
        }
        self.metadata.render_page(artifact, page)
    }
}

#[cfg(feature = "pdfium")]
pub struct PdfiumRenderBackend {
    pub target_width: i32,
    pub max_height: i32,
    pdfium: RefCell<Option<std::result::Result<Pdfium, String>>>,
}

#[cfg(feature = "pdfium")]
impl Default for PdfiumRenderBackend {
    fn default() -> Self {
        Self {
            target_width: PDFIUM_TARGET_WIDTH,
            max_height: PDFIUM_MAX_HEIGHT,
            pdfium: RefCell::new(None),
        }
    }
}

#[cfg(feature = "pdfium")]
impl fmt::Debug for PdfiumRenderBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PdfiumRenderBackend")
            .field("target_width", &self.target_width)
            .field("max_height", &self.max_height)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "pdfium")]
impl RenderBackend for PdfiumRenderBackend {
    fn page_count(&self, artifact: &DocumentArtifact) -> Result<usize> {
        if artifact.kind != ArtifactKind::Pdf {
            return Ok(0);
        }
        self.with_pdfium(|pdfium| {
            let document = pdfium
                .load_pdf_from_byte_vec(artifact.bytes.clone(), None)
                .map_err(to_engine_error)?;
            Ok(usize::from(document.pages().len()))
        })
    }

    fn render_page(&self, artifact: &DocumentArtifact, page: usize) -> Result<RenderedPage> {
        if artifact.kind != ArtifactKind::Pdf {
            return Err(EngineError::new(
                "Pdfium renderer only accepts PDF artifacts",
            ));
        }
        self.with_pdfium(|pdfium| {
            let document = pdfium
                .load_pdf_from_byte_vec(artifact.bytes.clone(), None)
                .map_err(to_engine_error)?;
            let page_index =
                u16::try_from(page).map_err(|_| EngineError::new("page index does not fit u16"))?;
            let page = document.pages().get(page_index).map_err(to_engine_error)?;
            let config = PdfRenderConfig::new()
                .set_target_width(self.target_width)
                .set_maximum_height(self.max_height)
                .render_form_data(true)
                .render_annotations(true);
            let bitmap = page.render_with_config(&config).map_err(to_engine_error)?;
            Ok(RenderedPage {
                index: usize::from(page_index),
                width: u32::try_from(bitmap.width())
                    .map_err(|_| EngineError::new("rendered PDF page width is invalid"))?,
                height: u32::try_from(bitmap.height())
                    .map_err(|_| EngineError::new("rendered PDF page height is invalid"))?,
                pixels_rgba: bitmap.as_rgba_bytes(),
            })
        })
    }
}

#[cfg(feature = "pdfium")]
impl PdfiumRenderBackend {
    fn with_pdfium<T>(&self, f: impl FnOnce(&Pdfium) -> Result<T>) -> Result<T> {
        if self.pdfium.borrow().is_none() {
            let pdfium = pdfium_auto::bind_pdfium_silent().map_err(|error| error.to_string());
            *self.pdfium.borrow_mut() = Some(pdfium);
        }

        let pdfium = self.pdfium.borrow();
        match pdfium.as_ref().expect("PDFium cache should be initialized") {
            Ok(pdfium) => f(pdfium),
            Err(error) => Err(EngineError::new(format!("PDFium is unavailable: {error}"))),
        }
    }
}

#[cfg(feature = "pdfium")]
fn to_engine_error(error: impl std::fmt::Display) -> EngineError {
    EngineError::new(error.to_string())
}

#[derive(Debug, Default)]
pub struct PdfMetadataRenderBackend;

impl PdfMetadataRenderBackend {
    pub fn is_valid_pdf(bytes: &[u8]) -> bool {
        bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .is_some_and(|start| bytes[start..].starts_with(b"%PDF-"))
    }

    pub fn count_pdf_pages(bytes: &[u8]) -> usize {
        let mut count = 0;
        let mut search_from = 0;
        while let Some(relative) = find_subslice(&bytes[search_from..], b"/Type") {
            let type_at = search_from + relative;
            let after_type = type_at + b"/Type".len();
            if matches_pdf_page_name(bytes, after_type) {
                count += 1;
            }
            search_from = after_type;
        }
        count
    }

    fn page_count_for_pdf(&self, artifact: &DocumentArtifact) -> Result<usize> {
        if artifact.bytes.is_empty() {
            return Ok(0);
        }
        if !Self::is_valid_pdf(&artifact.bytes) {
            return Err(EngineError::new(
                "PDF artifact does not start with a %PDF- header",
            ));
        }
        let counted_pages = Self::count_pdf_pages(&artifact.bytes);
        Ok(counted_pages.max(1))
    }
}

impl RenderBackend for PdfMetadataRenderBackend {
    fn page_count(&self, artifact: &DocumentArtifact) -> Result<usize> {
        match artifact.kind {
            ArtifactKind::Pdf => self.page_count_for_pdf(artifact),
            ArtifactKind::Xdv | ArtifactKind::Dvi => Ok(page_count_for_dvi_like(&artifact.bytes)),
            ArtifactKind::Unknown => Ok(0),
        }
    }

    fn render_page(&self, artifact: &DocumentArtifact, page: usize) -> Result<RenderedPage> {
        if self.page_count(artifact)? <= page {
            return Err(EngineError::new("page index is out of range"));
        }
        match artifact.kind {
            ArtifactKind::Dvi | ArtifactKind::Xdv => {
                if let Some(rendered) = render_dvi_like_page(&artifact.bytes, page) {
                    Ok(rendered)
                } else {
                    Ok(placeholder_page(page))
                }
            }
            _ => Ok(placeholder_page(page)),
        }
    }
}

fn page_count_for_dvi_like(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    count_dvi_like_pages(bytes).unwrap_or(0).max(1)
}

fn count_dvi_like_pages(bytes: &[u8]) -> Option<usize> {
    let mut cursor = DviCursor::new(bytes);
    let mut pages = 0;
    while let Some(opcode) = cursor.next_u8() {
        match opcode {
            0..=127 | 138 | 140..=142 | 147 | 152 | 157 | 162 | 166 | 171..=234 => {}
            128 | 133 | 143 | 148 | 153 | 158 | 163 | 167 | 235 => cursor.skip(1)?,
            129 | 134 | 144 | 149 | 154 | 159 | 164 | 168 | 236 => cursor.skip(2)?,
            130 | 135 | 145 | 150 | 155 | 160 | 165 | 169 | 237 => cursor.skip(3)?,
            131 | 136 | 146 | 151 | 156 | 161 | 170 | 238 => cursor.skip(4)?,
            132 | 137 => cursor.skip(8)?,
            139 => {
                pages += 1;
                cursor.skip(44)?;
            }
            239 => cursor.skip_prefixed_payload(1)?,
            240 => cursor.skip_prefixed_payload(2)?,
            241 => cursor.skip_prefixed_payload(3)?,
            242 => cursor.skip_prefixed_payload(4)?,
            243 => cursor.skip_font_def(1)?,
            244 => cursor.skip_font_def(2)?,
            245 => cursor.skip_font_def(3)?,
            246 => cursor.skip_font_def(4)?,
            247 => cursor.skip_preamble()?,
            248 => return Some(pages),
            249 => return Some(pages),
            _ => return None,
        }
    }
    Some(pages)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DviRule {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DviGlyph {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    codepoint: u32,
    font: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DviElement {
    Rule(DviRule),
    Glyph(DviGlyph),
}

#[derive(Debug, Clone, Copy, Default)]
struct DviState {
    h: i64,
    v: i64,
    w: i64,
    x: i64,
    y: i64,
    z: i64,
    font: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DviFont {
    id: i64,
    scaled_size: i64,
    design_size: i64,
    name: String,
}

fn render_dvi_like_page(bytes: &[u8], page: usize) -> Option<RenderedPage> {
    let elements = parse_dvi_like_elements(bytes, page)?;
    if elements.is_empty() {
        return None;
    }

    let mut rendered = placeholder_page(page);
    let bounds = element_bounds(&elements)?;
    for element in elements {
        paint_element(&mut rendered, element, bounds);
    }
    Some(rendered)
}

fn parse_dvi_like_elements(bytes: &[u8], target_page: usize) -> Option<Vec<DviElement>> {
    let mut cursor = DviCursor::new(bytes);
    let mut page = None;
    let mut state = DviState::default();
    let mut stack = Vec::new();
    let mut elements = Vec::new();
    let mut fonts = HashMap::new();

    while let Some(opcode) = cursor.next_u8() {
        match opcode {
            0..=127 => record_dvi_glyph(
                &mut elements,
                page,
                target_page,
                &mut state,
                &fonts,
                u32::from(opcode),
                true,
            ),
            128 => {
                let codepoint = cursor.read_uint(1)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    true,
                );
            }
            129 => {
                let codepoint = cursor.read_uint(2)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    true,
                );
            }
            130 => {
                let codepoint = cursor.read_uint(3)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    true,
                );
            }
            131 => {
                let codepoint = cursor.read_uint(4)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    true,
                );
            }
            132 | 137 => {
                let height = cursor.read_i32()?;
                let width = cursor.read_i32()?;
                if page == Some(target_page) && width > 0 && height > 0 {
                    elements.push(DviElement::Rule(DviRule {
                        x: state.h,
                        y: state.v - i64::from(height),
                        width: i64::from(width),
                        height: i64::from(height),
                    }));
                }
                if opcode == 132 {
                    state.h += i64::from(width);
                }
            }
            133 => {
                let codepoint = cursor.read_uint(1)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    false,
                );
            }
            134 => {
                let codepoint = cursor.read_uint(2)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    false,
                );
            }
            135 => {
                let codepoint = cursor.read_uint(3)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    false,
                );
            }
            136 => {
                let codepoint = cursor.read_uint(4)?;
                record_dvi_glyph(
                    &mut elements,
                    page,
                    target_page,
                    &mut state,
                    &fonts,
                    codepoint as u32,
                    false,
                );
            }
            138 => {}
            139 => {
                let next_page = page.map_or(0, |page| page + 1);
                page = Some(next_page);
                state = DviState::default();
                stack.clear();
                cursor.skip(44)?;
            }
            140 => {
                if page == Some(target_page) {
                    return Some(elements);
                }
            }
            141 => stack.push(state),
            142 => state = stack.pop()?,
            143 => state.h += cursor.read_int(1)?,
            144 => state.h += cursor.read_int(2)?,
            145 => state.h += cursor.read_int(3)?,
            146 => state.h += cursor.read_int(4)?,
            147 => state.h += state.w,
            148 => {
                state.w = cursor.read_int(1)?;
                state.h += state.w;
            }
            149 => {
                state.w = cursor.read_int(2)?;
                state.h += state.w;
            }
            150 => {
                state.w = cursor.read_int(3)?;
                state.h += state.w;
            }
            151 => {
                state.w = cursor.read_int(4)?;
                state.h += state.w;
            }
            152 => state.h += state.x,
            153 => {
                state.x = cursor.read_int(1)?;
                state.h += state.x;
            }
            154 => {
                state.x = cursor.read_int(2)?;
                state.h += state.x;
            }
            155 => {
                state.x = cursor.read_int(3)?;
                state.h += state.x;
            }
            156 => {
                state.x = cursor.read_int(4)?;
                state.h += state.x;
            }
            157 => state.v += cursor.read_int(1)?,
            158 => state.v += cursor.read_int(2)?,
            159 => state.v += cursor.read_int(3)?,
            160 => state.v += cursor.read_int(4)?,
            161 => state.v += state.y,
            162 => {
                state.y = cursor.read_int(1)?;
                state.v += state.y;
            }
            163 => {
                state.y = cursor.read_int(2)?;
                state.v += state.y;
            }
            164 => {
                state.y = cursor.read_int(3)?;
                state.v += state.y;
            }
            165 => {
                state.y = cursor.read_int(4)?;
                state.v += state.y;
            }
            166 => state.v += state.z,
            167 => {
                state.z = cursor.read_int(1)?;
                state.v += state.z;
            }
            168 => {
                state.z = cursor.read_int(2)?;
                state.v += state.z;
            }
            169 => {
                state.z = cursor.read_int(3)?;
                state.v += state.z;
            }
            170 => {
                state.z = cursor.read_int(4)?;
                state.v += state.z;
            }
            171..=234 => state.font = Some(i64::from(opcode - 171)),
            235 => {
                state.font = Some(cursor.read_uint(1)? as i64);
            }
            236 => {
                state.font = Some(cursor.read_uint(2)? as i64);
            }
            237 => {
                state.font = Some(cursor.read_uint(3)? as i64);
            }
            238 => {
                state.font = Some(cursor.read_uint(4)? as i64);
            }
            239 => cursor.skip_prefixed_payload(1)?,
            240 => cursor.skip_prefixed_payload(2)?,
            241 => cursor.skip_prefixed_payload(3)?,
            242 => cursor.skip_prefixed_payload(4)?,
            243 => {
                let font = cursor.read_font_def(1)?;
                fonts.insert(font.id, font);
            }
            244 => {
                let font = cursor.read_font_def(2)?;
                fonts.insert(font.id, font);
            }
            245 => {
                let font = cursor.read_font_def(3)?;
                fonts.insert(font.id, font);
            }
            246 => {
                let font = cursor.read_font_def(4)?;
                fonts.insert(font.id, font);
            }
            247 => cursor.skip_preamble()?,
            248 | 249 => return Some(elements),
            _ => return None,
        }
    }
    Some(elements)
}

fn record_dvi_glyph(
    elements: &mut Vec<DviElement>,
    page: Option<usize>,
    target_page: usize,
    state: &mut DviState,
    fonts: &HashMap<i64, DviFont>,
    codepoint: u32,
    advance: bool,
) {
    let (width, height) = glyph_dimensions(state.font.and_then(|id| fonts.get(&id)));
    if page == Some(target_page) {
        elements.push(DviElement::Glyph(DviGlyph {
            x: state.h,
            y: state.v - height,
            width,
            height,
            codepoint,
            font: state.font,
        }));
    }
    if advance {
        state.h += width;
    }
}

fn glyph_dimensions(font: Option<&DviFont>) -> (i64, i64) {
    let Some(font) = font else {
        return (DVI_GLYPH_WIDTH, DVI_GLYPH_HEIGHT);
    };
    let height = font.scaled_size.abs().clamp(250, 10_000);
    let name_bias = i64::try_from(font.name.len()).unwrap_or(0).clamp(0, 200);
    let design = font.design_size.abs().max(1);
    let width = ((height / 2) + (height / design).min(height / 4) + name_bias).clamp(200, 8_000);
    (width, height)
}

fn element_bounds(elements: &[DviElement]) -> Option<(i64, i64, i64, i64)> {
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    for element in elements {
        let (x, y, width, height) = match *element {
            DviElement::Rule(rule) => (rule.x, rule.y, rule.width, rule.height),
            DviElement::Glyph(glyph) => (glyph.x, glyph.y, glyph.width, glyph.height),
        };
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + width);
        max_y = max_y.max(y + height);
    }
    (min_x < max_x && min_y < max_y).then_some((min_x, min_y, max_x, max_y))
}

fn paint_element(page: &mut RenderedPage, element: DviElement, bounds: (i64, i64, i64, i64)) {
    match element {
        DviElement::Rule(rule) => {
            paint_box(page, rule.x, rule.y, rule.width, rule.height, bounds, 32)
        }
        DviElement::Glyph(glyph) => paint_box(
            page,
            glyph.x,
            glyph.y,
            glyph.width,
            glyph.height,
            bounds,
            glyph_shade(glyph),
        ),
    }
}

fn glyph_shade(glyph: DviGlyph) -> u8 {
    let font_bias = glyph.font.unwrap_or(0).unsigned_abs() as u32 % 32;
    55 + ((glyph.codepoint + font_bias) % 80) as u8
}

fn paint_box(
    page: &mut RenderedPage,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    bounds: (i64, i64, i64, i64),
    shade: u8,
) {
    let (min_x, min_y, max_x, max_y) = bounds;
    let margin = 8.0;
    let usable_width = (page.width as f32 - margin * 2.0).max(1.0);
    let usable_height = (page.height as f32 - margin * 2.0).max(1.0);
    let scale_x = usable_width / (max_x - min_x).max(1) as f32;
    let scale_y = usable_height / (max_y - min_y).max(1) as f32;
    let scale = scale_x.min(scale_y).max(1.0 / 1024.0);

    let left = margin + (x - min_x) as f32 * scale;
    let top = margin + (y - min_y) as f32 * scale;
    let right = left + (width as f32 * scale).max(1.0);
    let bottom = top + (height as f32 * scale).max(1.0);

    let x0 = left.floor().clamp(0.0, page.width as f32) as u32;
    let y0 = top.floor().clamp(0.0, page.height as f32) as u32;
    let x1 = right.ceil().clamp(0.0, page.width as f32) as u32;
    let y1 = bottom.ceil().clamp(0.0, page.height as f32) as u32;

    for y in y0..y1 {
        for x in x0..x1 {
            let offset = ((y * page.width + x) * 4) as usize;
            page.pixels_rgba[offset] = shade;
            page.pixels_rgba[offset + 1] = shade;
            page.pixels_rgba[offset + 2] = shade;
            page.pixels_rgba[offset + 3] = 255;
        }
    }
}

struct DviCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> DviCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn next_u8(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.offset)?;
        self.offset += 1;
        Some(byte)
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        self.offset = self.offset.checked_add(count)?;
        (self.offset <= self.bytes.len()).then_some(())
    }

    fn read_uint(&mut self, count: usize) -> Option<usize> {
        let end = self.offset.checked_add(count)?;
        let bytes = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(
            bytes
                .iter()
                .fold(0usize, |value, byte| (value << 8) | usize::from(*byte)),
        )
    }

    fn read_bytes(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(count)?;
        let bytes = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(bytes)
    }

    fn read_int(&mut self, count: usize) -> Option<i64> {
        let end = self.offset.checked_add(count)?;
        let bytes = self.bytes.get(self.offset..end)?;
        self.offset = end;
        if bytes.is_empty() {
            return Some(0);
        }
        let mut value = 0i64;
        for byte in bytes {
            value = (value << 8) | i64::from(*byte);
        }
        let sign_bit = 1i64 << (count * 8 - 1);
        if value & sign_bit != 0 {
            value -= 1i64 << (count * 8);
        }
        Some(value)
    }

    fn read_i32(&mut self) -> Option<i32> {
        i32::try_from(self.read_int(4)?).ok()
    }

    fn skip_prefixed_payload(&mut self, len_bytes: usize) -> Option<()> {
        let len = self.read_uint(len_bytes)?;
        self.skip(len)
    }

    fn skip_font_def(&mut self, font_id_bytes: usize) -> Option<()> {
        self.skip(font_id_bytes + 12)?;
        let area_len = usize::from(self.next_u8()?);
        let name_len = usize::from(self.next_u8()?);
        self.skip(area_len.checked_add(name_len)?)
    }

    fn read_font_def(&mut self, font_id_bytes: usize) -> Option<DviFont> {
        let id = self.read_uint(font_id_bytes)? as i64;
        self.skip(4)?;
        let scaled_size = self.read_int(4)?;
        let design_size = self.read_int(4)?;
        let area_len = usize::from(self.next_u8()?);
        let name_len = usize::from(self.next_u8()?);
        let name_bytes = self.read_bytes(area_len.checked_add(name_len)?)?;
        Some(DviFont {
            id,
            scaled_size,
            design_size,
            name: String::from_utf8_lossy(name_bytes).into_owned(),
        })
    }

    fn skip_preamble(&mut self) -> Option<()> {
        self.skip(13)?;
        let comment_len = usize::from(self.next_u8()?);
        self.skip(comment_len)
    }
}

#[derive(Debug, Default)]
pub struct StubRenderBackend;

impl RenderBackend for StubRenderBackend {
    fn page_count(&self, artifact: &DocumentArtifact) -> Result<usize> {
        AutoRenderBackend::default().page_count(artifact)
    }

    fn render_page(&self, artifact: &DocumentArtifact, page: usize) -> Result<RenderedPage> {
        AutoRenderBackend::default().render_page(artifact, page)
    }
}

fn placeholder_page(index: usize) -> RenderedPage {
    let mut pixels_rgba = vec![255; (PLACEHOLDER_WIDTH * PLACEHOLDER_HEIGHT * 4) as usize];
    for y in 0..PLACEHOLDER_HEIGHT {
        for x in 0..PLACEHOLDER_WIDTH {
            let offset = ((y * PLACEHOLDER_WIDTH + x) * 4) as usize;
            let border =
                x == 0 || y == 0 || x + 1 == PLACEHOLDER_WIDTH || y + 1 == PLACEHOLDER_HEIGHT;
            if border {
                pixels_rgba[offset] = 210;
                pixels_rgba[offset + 1] = 214;
                pixels_rgba[offset + 2] = 220;
            }
            pixels_rgba[offset + 3] = 255;
        }
    }
    RenderedPage {
        index,
        width: PLACEHOLDER_WIDTH,
        height: PLACEHOLDER_HEIGHT,
        pixels_rgba,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn matches_pdf_page_name(bytes: &[u8], mut index: usize) -> bool {
    index = skip_pdf_whitespace_and_comments(bytes, index);
    let Some(rest) = bytes.get(index..) else {
        return false;
    };
    if !rest.starts_with(b"/Page") {
        return false;
    }
    let after_page = index + b"/Page".len();
    bytes
        .get(after_page)
        .is_none_or(|byte| is_pdf_delimiter_or_whitespace(*byte))
}

fn skip_pdf_whitespace_and_comments(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            index += 1;
        }
        if bytes.get(index) != Some(&b'%') {
            return index;
        }
        while bytes
            .get(index)
            .is_some_and(|byte| *byte != b'\r' && *byte != b'\n')
        {
            index += 1;
        }
    }
}

fn is_pdf_delimiter_or_whitespace(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf(bytes: &[u8]) -> DocumentArtifact {
        DocumentArtifact {
            kind: ArtifactKind::Pdf,
            bytes: bytes.to_vec(),
            source_name: Some("test.pdf".to_string()),
        }
    }

    fn dvi_like(kind: ArtifactKind, bytes: &[u8]) -> DocumentArtifact {
        DocumentArtifact {
            kind,
            bytes: bytes.to_vec(),
            source_name: Some("test.dvi".to_string()),
        }
    }

    fn minimal_dvi_with_pages(pages: usize) -> Vec<u8> {
        let mut bytes = vec![247, 2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 3, 232, 0];
        for _ in 0..pages {
            bytes.push(139);
            bytes.extend([0; 44]);
            bytes.push(140);
        }
        bytes.push(248);
        bytes
    }

    fn minimal_dvi_with_rule() -> Vec<u8> {
        let mut bytes = vec![247, 2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 3, 232, 0];
        bytes.push(139);
        bytes.extend([0; 44]);
        bytes.push(132);
        bytes.extend(1000i32.to_be_bytes());
        bytes.extend(2000i32.to_be_bytes());
        bytes.push(140);
        bytes.push(248);
        bytes
    }

    fn minimal_dvi_with_glyph() -> Vec<u8> {
        let mut bytes = vec![247, 2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 3, 232, 0];
        bytes.push(139);
        bytes.extend([0; 44]);
        bytes.push(b'A');
        bytes.push(140);
        bytes.push(248);
        bytes
    }

    fn minimal_dvi_with_font_sized_glyph() -> Vec<u8> {
        let mut bytes = vec![247, 2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 3, 232, 0];
        bytes.push(243);
        bytes.push(1);
        bytes.extend(0i32.to_be_bytes());
        bytes.extend(3000i32.to_be_bytes());
        bytes.extend(1000i32.to_be_bytes());
        bytes.push(0);
        bytes.push(2);
        bytes.extend(b"cm");
        bytes.push(139);
        bytes.extend([0; 44]);
        bytes.push(172);
        bytes.push(b'A');
        bytes.push(140);
        bytes.push(248);
        bytes
    }

    #[test]
    fn validates_pdf_header() {
        assert!(PdfMetadataRenderBackend::is_valid_pdf(b"%PDF-1.7\n"));
        assert!(PdfMetadataRenderBackend::is_valid_pdf(b"\n%PDF-1.5\n"));
        assert!(!PdfMetadataRenderBackend::is_valid_pdf(b"not pdf"));
    }

    #[test]
    fn counts_page_objects_without_counting_pages_tree() {
        let bytes = br#"%PDF-1.7
1 0 obj
<< /Type /Pages /Count 2 /Kids [2 0 R 3 0 R] >>
endobj
2 0 obj
<< /Type /Page /Parent 1 0 R >>
endobj
3 0 obj
<< /Type
   /Page
   /Parent 1 0 R >>
endobj
"#;
        assert_eq!(PdfMetadataRenderBackend::count_pdf_pages(bytes), 2);
    }

    #[test]
    fn page_count_rejects_invalid_pdf_bytes() {
        let backend = PdfMetadataRenderBackend;
        assert!(backend.page_count(&pdf(b"nope")).is_err());
    }

    #[test]
    fn valid_pdf_without_visible_page_objects_keeps_one_placeholder_page() {
        let backend = PdfMetadataRenderBackend;
        assert_eq!(backend.page_count(&pdf(b"%PDF-1.7\n%%EOF\n")).unwrap(), 1);
    }

    #[test]
    fn render_page_returns_stable_placeholder() {
        let backend = PdfMetadataRenderBackend;
        let page = backend
            .render_page(&pdf(b"%PDF-1.7\n<< /Type /Page >>"), 0)
            .unwrap();
        assert_eq!(page.index, 0);
        assert_eq!(page.width, PLACEHOLDER_WIDTH);
        assert_eq!(page.height, PLACEHOLDER_HEIGHT);
        assert_eq!(
            page.pixels_rgba.len(),
            (PLACEHOLDER_WIDTH * PLACEHOLDER_HEIGHT * 4) as usize
        );
    }

    #[test]
    fn auto_backend_falls_back_to_metadata_placeholder() {
        let backend = AutoRenderBackend::default();
        let artifact = pdf(b"%PDF-1.7\n<< /Type /Page >>");
        assert_eq!(backend.page_count(&artifact).unwrap(), 1);
        let page = backend.render_page(&artifact, 0).unwrap();
        assert_eq!(page.width, PLACEHOLDER_WIDTH);
        assert_eq!(page.height, PLACEHOLDER_HEIGHT);
    }

    #[test]
    fn counts_dvi_like_bop_opcodes_as_pages() {
        let backend = PdfMetadataRenderBackend;
        let bytes = minimal_dvi_with_pages(2);
        assert_eq!(
            backend
                .page_count(&dvi_like(ArtifactKind::Dvi, &bytes))
                .unwrap(),
            2
        );
        assert_eq!(
            backend
                .page_count(&dvi_like(ArtifactKind::Xdv, &bytes))
                .unwrap(),
            2
        );
    }

    #[test]
    fn dvi_like_page_count_ignores_bop_byte_inside_payload() {
        let backend = PdfMetadataRenderBackend;
        let mut bytes = vec![247, 2, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 3, 232, 0, 239, 1, 139];
        bytes.extend(minimal_dvi_with_pages(1).into_iter().skip(15));
        assert_eq!(
            backend
                .page_count(&dvi_like(ArtifactKind::Dvi, &bytes))
                .unwrap(),
            1
        );
    }

    #[test]
    fn nonempty_dvi_like_without_visible_bop_keeps_one_placeholder_page() {
        let backend = PdfMetadataRenderBackend;
        assert_eq!(
            backend
                .page_count(&dvi_like(ArtifactKind::Dvi, &[247, 2, 248]))
                .unwrap(),
            1
        );
        assert_eq!(
            backend
                .page_count(&dvi_like(ArtifactKind::Dvi, &[]))
                .unwrap(),
            0
        );
    }

    #[test]
    fn dvi_like_rule_opcodes_render_dark_pixels() {
        let backend = PdfMetadataRenderBackend;
        let page = backend
            .render_page(&dvi_like(ArtifactKind::Dvi, &minimal_dvi_with_rule()), 0)
            .unwrap();
        let dark_pixels = page
            .pixels_rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[0] < 80 && pixel[1] < 80 && pixel[2] < 80)
            .count();
        assert!(dark_pixels > 0);
    }

    #[test]
    fn dvi_like_glyph_opcodes_render_placeholder_marks() {
        let backend = PdfMetadataRenderBackend;
        let page = backend
            .render_page(&dvi_like(ArtifactKind::Dvi, &minimal_dvi_with_glyph()), 0)
            .unwrap();
        let glyph_pixels = page
            .pixels_rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[0] < 150 && pixel[1] < 150 && pixel[2] < 150)
            .count();
        assert!(glyph_pixels > 0);
    }

    #[test]
    fn dvi_like_font_def_scales_glyph_placeholders() {
        let elements = parse_dvi_like_elements(&minimal_dvi_with_font_sized_glyph(), 0).unwrap();
        let glyph = elements
            .iter()
            .find_map(|element| match element {
                DviElement::Glyph(glyph) => Some(*glyph),
                DviElement::Rule(_) => None,
            })
            .unwrap();

        assert_eq!(glyph.font, Some(1));
        assert_eq!(glyph.height, 3000);
        assert!(glyph.width > DVI_GLYPH_WIDTH);
    }

    #[cfg(feature = "pdfium")]
    #[test]
    fn pdfium_smoke_renders_real_pdf_when_requested() {
        let Some(path) = std::env::var_os("OXIPRESSO_PDFIUM_SMOKE_PDF") else {
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let artifact = DocumentArtifact {
            kind: ArtifactKind::Pdf,
            bytes,
            source_name: Some(path.to_string_lossy().to_string()),
        };
        let backend = PdfiumRenderBackend::default();
        let page_count = backend.page_count(&artifact).unwrap();
        assert!(page_count >= 1);
        let page = backend.render_page(&artifact, 0).unwrap();
        assert!(page.width > PLACEHOLDER_WIDTH);
        assert!(page.height > PLACEHOLDER_HEIGHT);
        assert_eq!(
            page.pixels_rgba.len(),
            (page.width * page.height * 4) as usize
        );
    }
}
