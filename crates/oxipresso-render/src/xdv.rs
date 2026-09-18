//! XDV/DVI stream parser producing positioned display-list elements.
//!
//! Layouts follow the original TeXpresso engine output (`xetex-shipout.c`,
//! `xetex-ext.c`) and its own reader (`src/dvi/mydvi_opcodes.h`): classic DVI
//! opcodes, plus the XeTeX extensions `DEFINE_NATIVE_FONT` (252),
//! `SET_GLYPHS` (253), and `SET_TEXT_AND_GLYPHS` (254). Page dimensions come
//! from the synthesized `pdf:pagesize` special emitted after every BOP.

use std::collections::HashMap;

use oxipresso_engine_api::{EngineError, Result};

/// Fallback page size (points) when the `pdf:pagesize` special is missing.
pub const DEFAULT_PAGE_WIDTH_PT: f64 = 614.295;
pub const DEFAULT_PAGE_HEIGHT_PT: f64 = 794.97;

#[derive(Debug, Clone)]
pub struct XdvDocument {
    pub pages: Vec<XdvPage>,
    pub fonts: HashMap<u32, XdvFont>,
    /// Points per DVI unit derived from the preamble ratio.
    pub pt_per_unit: f64,
    pub mag: u32,
}

impl XdvDocument {
    /// Stable content digest of one page: hashes the page geometry, the font
    /// definitions it references (name, size, color), and every element
    /// (glyph codes and absolute positions, rule rectangles). Two pages with
    /// equal digests render identically, which lets the renderer skip
    /// unchanged pages after a rebuild.
    pub fn page_digest(&self, index: usize) -> Option<u64> {
        let page = self.pages.get(index)?;
        let mut hash = FnvHasher::new();
        hash.write_u64(page.width_pt.to_bits());
        hash.write_u64(page.height_pt.to_bits());
        hash.write_u64(page.page_number as u64);
        let mut referenced: Vec<u32> = Vec::new();
        for element in &page.elements {
            if let XdvElement::Glyphs { font_id, .. } = element {
                referenced.push(*font_id);
            }
        }
        referenced.sort_unstable();
        referenced.dedup();
        for font_id in referenced {
            if let Some(font) = self.fonts.get(&font_id) {
                hash.write_u64(font_id as u64);
                hash.write(font.name.as_bytes());
                hash.write_u8(0xff);
                hash.write_u64(font.size_pt.to_bits());
                hash.write_u64(font.color_rgba.unwrap_or(0) as u64);
                hash.write_u64(font.extend.to_bits());
                hash.write_u64(font.slant.to_bits());
                hash.write_u64(font.embolden.to_bits());
            }
        }
        hash.write_u64(page.elements.len() as u64);
        for element in &page.elements {
            match element {
                XdvElement::Glyphs {
                    font_id,
                    color_rgba,
                    glyphs,
                } => {
                    hash.write_u8(1);
                    hash.write_u64(*font_id as u64);
                    hash.write_u64(color_rgba.unwrap_or(0) as u64);
                    hash.write_u64(glyphs.len() as u64);
                    for glyph in glyphs {
                        hash.write_u64(glyph.code as u64);
                        hash.write_u64(glyph.x_pt.to_bits());
                        hash.write_u64(glyph.y_pt.to_bits());
                    }
                }
                XdvElement::Rule {
                    x_pt,
                    y_pt,
                    w_pt,
                    h_pt,
                } => {
                    hash.write_u8(2);
                    hash.write_u64(x_pt.to_bits());
                    hash.write_u64(y_pt.to_bits());
                    hash.write_u64(w_pt.to_bits());
                    hash.write_u64(h_pt.to_bits());
                }
                XdvElement::Image {
                    x_pt,
                    y_pt,
                    w_pt,
                    h_pt,
                    scale,
                    path,
                } => {
                    hash.write_u8(3);
                    hash.write_u64(x_pt.to_bits());
                    hash.write_u64(y_pt.to_bits());
                    hash.write_u64(w_pt.to_bits());
                    hash.write_u64(h_pt.to_bits());
                    hash.write_u64(
                        scale
                            .map(|[sx, sy]| sx.to_bits() ^ sy.to_bits().rotate_left(32))
                            .unwrap_or(0),
                    );
                    hash.write(path.as_bytes());
                }
            }
        }
        Some(hash.finish())
    }
}

/// FNV-1a 64-bit hasher shared by page digests and artifact caches.
pub(crate) struct FnvHasher {
    hash: u64,
}

impl FnvHasher {
    pub(crate) fn new() -> Self {
        Self {
            hash: 0xcbf2_9ce4_8422_2325,
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(0x1000_0000_01b3);
        }
    }

    pub(crate) fn write_u8(&mut self, value: u8) {
        self.write(&[value]);
    }

    pub(crate) fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    pub(crate) fn finish(self) -> u64 {
        self.hash
    }
}

#[derive(Debug, Clone, Default)]
pub struct XdvPage {
    /// Page width in points (from the `pdf:pagesize` special).
    pub width_pt: f64,
    /// Page height in points (from the `pdf:pagesize` special).
    pub height_pt: f64,
    pub page_number: i32,
    pub elements: Vec<XdvElement>,
}

#[derive(Debug, Clone)]
pub struct XdvFont {
    pub id: u32,
    /// True for XDV native fonts (opcode 252), false for classic DVI fonts.
    pub native: bool,
    /// Font file name carried by the XDV stream (without directory).
    pub name: String,
    /// Scaled size in points (16.16 Fixed for native fonts, 20.12 for classic).
    pub size_pt: f64,
    pub design_size_pt: f64,
    pub face_index: u32,
    pub color_rgba: Option<u32>,
    /// Horizontal extend factor (1.0 = none), from XDV_FLAG_EXTEND.
    pub extend: f64,
    /// Slant factor (0.0 = upright), from XDV_FLAG_SLANT.
    pub slant: f64,
    /// Embolden strength (0.0 = none), from XDV_FLAG_EMBOLDEN.
    pub embolden: f64,
    /// Classic fonts only: TFM widths (fix_word) per char code slot.
    pub tfm_widths: Option<Vec<i64>>,
}

#[derive(Debug, Clone)]
pub enum XdvElement {
    /// A run of glyphs from one font at absolute page positions in points.
    Glyphs {
        font_id: u32,
        /// Color override captured from `color push` specials active when the
        /// glyphs were emitted (`0xRRGGBBAA`).
        color_rgba: Option<u32>,
        glyphs: Vec<XdvGlyph>,
    },
    /// A filled rectangle (only positive widths/heights are emitted).
    Rule {
        x_pt: f64,
        y_pt: f64,
        w_pt: f64,
        h_pt: f64,
    },
    /// An image placed by a `pdf:image` special (`\includegraphics` /
    /// `\XeTeXpicfile`). `x_pt`/`y_pt` is the image's top-left corner on the
    /// page. The bbox form carries the display size directly in `w_pt`/`h_pt`;
    /// the matrix form carries `scale` (x, y) instead and the renderer computes
    /// the display size from the decoded image's native pixel size (1px = 1bp).
    Image {
        x_pt: f64,
        y_pt: f64,
        w_pt: f64,
        h_pt: f64,
        scale: Option<[f64; 2]>,
        /// Image file path exactly as written in the special.
        path: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct XdvGlyph {
    /// Classic DVI fonts: the char code. Native fonts: the glyph ID.
    pub code: u32,
    pub x_pt: f64,
    pub y_pt: f64,
}

/// TFM lookup callback: given the font name from an fnt_def (e.g. `cmr10`),
/// return the TFM file bytes so classic set_char advances can be tracked.
pub type TfmLookup<'a> = dyn FnMut(&str) -> Option<Vec<u8>> + 'a;

const SET_CHAR_MAX: u8 = 127;
const SET1: u8 = 128;
const SET4: u8 = 131;
const SET_RULE: u8 = 132;
const PUT1: u8 = 133;
const PUT4: u8 = 136;
const PUT_RULE: u8 = 137;
const NOP: u8 = 138;
const BOP: u8 = 139;
const EOP: u8 = 140;
const PUSH: u8 = 141;
const POP: u8 = 142;
const RIGHT1: u8 = 143;
const RIGHT4: u8 = 146;
const W0: u8 = 147;
const W1: u8 = 148;
const W4: u8 = 151;
const X0: u8 = 152;
const X1: u8 = 153;
const X4: u8 = 156;
const DOWN1: u8 = 157;
const DOWN4: u8 = 160;
const Y0: u8 = 161;
const Y1: u8 = 162;
const Y4: u8 = 165;
const Z0: u8 = 166;
const Z1: u8 = 167;
const Z4: u8 = 170;
const FNT_NUM_0: u8 = 171;
const FNT_NUM_MAX: u8 = 234;
const FNT1: u8 = 235;
const FNT4: u8 = 238;
const XXX1: u8 = 239;
const XXX4: u8 = 242;
const FNT_DEF1: u8 = 243;
const FNT_DEF4: u8 = 246;
const PRE: u8 = 247;
const POST: u8 = 248;
const POST_POST: u8 = 249;
const DEFINE_NATIVE_FONT: u8 = 252;
const SET_GLYPHS: u8 = 253;
const SET_TEXT_AND_GLYPHS: u8 = 254;

const XDV_FLAG_COLORED: u16 = 0x0200;
const XDV_FLAG_VARIATIONS: u16 = 0x0800;
const XDV_FLAG_EXTEND: u16 = 0x1000;
const XDV_FLAG_SLANT: u16 = 0x2000;
const XDV_FLAG_EMBOLDEN: u16 = 0x4000;

pub fn parse_xdv(bytes: &[u8], tfm_lookup: &mut TfmLookup<'_>) -> Result<XdvDocument> {
    let mut reader = Reader {
        bytes,
        pos: 0,
        last_width: 0,
    };
    let mut fonts: HashMap<u32, XdvFont> = HashMap::new();
    let mut pages: Vec<XdvPage> = Vec::new();
    let mut pt_per_unit = 1.0 / 65536.0;
    let mut mag = 1000u32;

    let mut stack: Vec<State> = Vec::new();
    let mut state = State::default();
    let mut current_font: Option<u32> = None;
    let mut page: Option<XdvPage> = None;
    let mut pending_glyphs: Option<(u32, Option<u32>, Vec<XdvGlyph>)> = None;
    let mut color_stack: Vec<u32> = Vec::new();
    let mut current_color: Option<u32> = None;

    macro_rules! flush_glyphs {
        () => {
            if let Some((font_id, color, glyphs)) = pending_glyphs.take()
                && !glyphs.is_empty()
                && let Some(target) = page.as_mut()
            {
                target.elements.push(XdvElement::Glyphs {
                    font_id,
                    color_rgba: color,
                    glyphs,
                });
            }
        };
    }

    while reader.pos < bytes.len() {
        let opcode = reader.u8()?;
        match opcode {
            PRE => {
                let id = reader.u8()?;
                if id != 7 {
                    return Err(EngineError::new(format!(
                        "unsupported DVI/XDV id byte {id}; only XDV (7) is supported"
                    )));
                }
                let num = reader.u32()? as f64;
                let den = reader.u32()? as f64;
                mag = reader.u32()?;
                pt_per_unit = (num / den) * 1e-4 * (72.27 / 25.4);
                let comment_len = reader.u8()?;
                reader.skip(comment_len as usize)?;
            }
            BOP => {
                flush_glyphs!();
                let mut page_number = 0i32;
                for index in 0..10 {
                    let count = reader.i32()?;
                    if index == 0 {
                        page_number = count;
                    }
                }
                let _prev_bop = reader.i32()?;
                stack.clear();
                state = State::default();
                current_font = None;
                page = Some(XdvPage {
                    width_pt: DEFAULT_PAGE_WIDTH_PT,
                    height_pt: DEFAULT_PAGE_HEIGHT_PT,
                    page_number,
                    elements: Vec::new(),
                });
            }
            EOP => {
                flush_glyphs!();
                if let Some(finished) = page.take() {
                    pages.push(finished);
                }
            }
            PUSH => {
                stack.push(state);
            }
            POP => {
                state = stack
                    .pop()
                    .ok_or_else(|| EngineError::new("DVI pop with empty stack"))?;
            }
            SET_RULE | PUT_RULE => {
                flush_glyphs!();
                let w = reader.i32()?;
                let h = reader.i32()?;
                if w > 0
                    && h > 0
                    && let Some(target) = page.as_mut()
                {
                    target.elements.push(XdvElement::Rule {
                        x_pt: state.h as f64 * pt_per_unit,
                        y_pt: state.v as f64 * pt_per_unit,
                        w_pt: w as f64 * pt_per_unit,
                        h_pt: h as f64 * pt_per_unit,
                    });
                }
                if opcode == SET_RULE {
                    state.h += w as i64;
                }
            }
            RIGHT1..=RIGHT4 => {
                let n = reader.sized_i32(opcode - RIGHT1 + 1)?;
                state.h += n as i64;
            }
            W0 => {
                state.h += state.w;
            }
            W1..=W4 => {
                state.w = reader.sized_i32(opcode - W1 + 1)? as i64;
                state.h += state.w;
            }
            X0 => {
                state.h += state.x;
            }
            X1..=X4 => {
                state.x = reader.sized_i32(opcode - X1 + 1)? as i64;
                state.h += state.x;
            }
            DOWN1..=DOWN4 => {
                let n = reader.sized_i32(opcode - DOWN1 + 1)?;
                state.v += n as i64;
            }
            Y0 => {
                state.v += state.y;
            }
            Y1..=Y4 => {
                state.y = reader.sized_i32(opcode - Y1 + 1)? as i64;
                state.v += state.y;
            }
            Z0 => {
                state.v += state.z;
            }
            Z1..=Z4 => {
                state.z = reader.sized_i32(opcode - Z1 + 1)? as i64;
                state.v += state.z;
            }
            FNT_NUM_0..=FNT_NUM_MAX => {
                current_font = Some((opcode - FNT_NUM_0) as u32);
            }
            FNT1..=FNT4 => {
                current_font = Some(reader.sized_u32(opcode - FNT1 + 1)?);
            }
            FNT_DEF1..=FNT_DEF4 => {
                let font = parse_classic_font_def(&mut reader, opcode - FNT_DEF1 + 1)?;
                fonts.insert(font.id, font);
            }
            DEFINE_NATIVE_FONT => {
                let font = parse_native_font_def(&mut reader)?;
                fonts.insert(font.id, font);
            }
            XXX1..=XXX4 => {
                let len = reader.sized_u32(opcode - XXX1 + 1)? as usize;
                let text = String::from_utf8_lossy(reader.take(len)?).to_string();
                if let Some(target) = page.as_mut()
                    && let Some(dims) = parse_pagesize_special(&text)
                {
                    target.width_pt = dims.0;
                    target.height_pt = dims.1;
                }
                if let Some(image) = parse_pdf_image_special(&text) {
                    let (w_pt, h_pt, scale) = resolve_image_size(&image);
                    flush_glyphs!();
                    if let Some(target) = page.as_mut() {
                        target.elements.push(XdvElement::Image {
                            x_pt: state.h as f64 * pt_per_unit,
                            y_pt: state.v as f64 * pt_per_unit,
                            w_pt,
                            h_pt,
                            scale,
                            path: image.path,
                        });
                    }
                }
                match parse_color_special(&text) {
                    Some(ColorSpecial::Push(rgba)) => {
                        if current_color != Some(rgba) {
                            // A color change splits the pending glyph run.
                            flush_glyphs!();
                        }
                        if let Some(previous) = current_color {
                            color_stack.push(previous);
                        }
                        current_color = Some(rgba);
                    }
                    Some(ColorSpecial::Pop) => {
                        flush_glyphs!();
                        current_color = color_stack.pop();
                    }
                    None => {}
                }
            }
            SET_GLYPHS => {
                let font_id = current_font
                    .ok_or_else(|| EngineError::new("XDV SET_GLYPHS before font selection"))?;
                let glyphs = read_glyph_array(&mut reader, state.h, state.v, pt_per_unit)?;
                if let Some(target) = page.as_mut() {
                    target.elements.push(XdvElement::Glyphs {
                        font_id,
                        color_rgba: current_color,
                        glyphs,
                    });
                }
                state.h += reader.last_width;
            }
            SET_TEXT_AND_GLYPHS => {
                let font_id = current_font.ok_or_else(|| {
                    EngineError::new("XDV SET_TEXT_AND_GLYPHS before font selection")
                })?;
                let text_len = reader.u16()? as usize;
                reader.skip(text_len * 2)?;
                let glyphs = read_glyph_array(&mut reader, state.h, state.v, pt_per_unit)?;
                if let Some(target) = page.as_mut() {
                    target.elements.push(XdvElement::Glyphs {
                        font_id,
                        color_rgba: current_color,
                        glyphs,
                    });
                }
                state.h += reader.last_width;
            }
            POST => {
                // Postamble: pages were already collected inline; the font
                // definitions that follow are handled by the main loop.
                reader.skip(4 * 6)?;
                reader.skip(2 * 2)?;
            }
            POST_POST => {
                break;
            }
            NOP => {}
            0..=SET_CHAR_MAX => {
                let font_id = current_font
                    .ok_or_else(|| EngineError::new("DVI set_char before font selection"))?;
                let code = opcode as u32;
                push_classic_glyph(
                    &mut pending_glyphs,
                    font_id,
                    current_color,
                    code,
                    state,
                    pt_per_unit,
                );
                state.h += char_width_dvi(&mut fonts, font_id, code, tfm_lookup);
            }
            SET1..=SET4 => {
                let font_id = current_font
                    .ok_or_else(|| EngineError::new("DVI set before font selection"))?;
                let code = reader.sized_u32(opcode - SET1 + 1)?;
                push_classic_glyph(
                    &mut pending_glyphs,
                    font_id,
                    current_color,
                    code,
                    state,
                    pt_per_unit,
                );
                state.h += char_width_dvi(&mut fonts, font_id, code, tfm_lookup);
            }
            PUT1..=PUT4 => {
                let font_id = current_font
                    .ok_or_else(|| EngineError::new("DVI put before font selection"))?;
                let code = reader.sized_u32(opcode - PUT1 + 1)?;
                push_classic_glyph(
                    &mut pending_glyphs,
                    font_id,
                    current_color,
                    code,
                    state,
                    pt_per_unit,
                );
            }
            other => {
                return Err(EngineError::new(format!(
                    "unsupported DVI/XDV opcode {other} at offset {}",
                    reader.pos
                )));
            }
        }
    }

    Ok(XdvDocument {
        pages,
        fonts,
        pt_per_unit,
        mag,
    })
}

/// Reads the shared `SET_GLYPHS`/`SET_TEXT_AND_GLYPHS` payload:
/// width[4], glyphCount[2], count × (x[4], y[4]) fixed-point offsets, then
/// count × uint16 glyph IDs. Positions become absolute points.
fn read_glyph_array(
    reader: &mut Reader<'_>,
    h: i64,
    v: i64,
    pt_per_unit: f64,
) -> Result<Vec<XdvGlyph>> {
    let width = reader.i32()?;
    reader.last_width = width as i64;
    let glyph_count = reader.u16()?;
    let mut offsets = Vec::with_capacity(glyph_count as usize);
    for _ in 0..glyph_count {
        let gx = reader.i32()?;
        let gy = reader.i32()?;
        offsets.push((gx, gy));
    }
    let mut glyphs = Vec::with_capacity(glyph_count as usize);
    for (gx, gy) in offsets {
        let code = reader.u16()? as u32;
        glyphs.push(XdvGlyph {
            code,
            x_pt: (h + gx as i64) as f64 * pt_per_unit,
            y_pt: (v + gy as i64) as f64 * pt_per_unit,
        });
    }
    Ok(glyphs)
}

fn push_classic_glyph(
    pending: &mut Option<(u32, Option<u32>, Vec<XdvGlyph>)>,
    font_id: u32,
    color: Option<u32>,
    code: u32,
    state: State,
    pt_per_unit: f64,
) {
    if !matches!(pending, Some((pending_font, _, _)) if *pending_font == font_id) {
        *pending = Some((font_id, color, Vec::new()));
    }
    if let Some((_, glyphs_color, glyphs)) = pending.as_mut() {
        if *glyphs_color != color {
            // Color changed without a flush (defensive); update the run.
            *glyphs_color = color;
        }
        glyphs.push(XdvGlyph {
            code,
            x_pt: state.h as f64 * pt_per_unit,
            y_pt: state.v as f64 * pt_per_unit,
        });
    }
}

/// Convert xcolor `hsb` (hue/saturation/brightness, each in [0,1]) to linear
/// RGB in [0,1]. Uses the standard hexcone HSV formula.
fn hsb_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    // xcolor hue is [0,1]; wrap into [0,1) then scale to the 6 hue sectors.
    let h = h - h.floor();
    let sector = h * 6.0;
    let i = sector.floor() as i32;
    let f = sector - sector.floor();
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

enum ColorSpecial {
    Push(u32),
    Pop,
}

/// Parses `color push <model> <values...>` / `color pop` specials from the
/// LaTeX color package. Returns `None` for any other special. Colors are
/// normalized to `0xRRGGBBAA`.
fn parse_color_special(text: &str) -> Option<ColorSpecial> {
    let rest = text.trim().strip_prefix("color ")?;
    let mut tokens = rest.split_whitespace();
    match tokens.next()? {
        "push" => {
            let model = tokens.next()?;
            let values: Vec<f64> = tokens
                .by_ref()
                .filter_map(|token| token.parse::<f64>().ok())
                .collect();
            let channel = |value: f64| ((value.clamp(0.0, 1.0)) * 255.0).round() as u32;
            let rgba = match model {
                "rgb" if values.len() >= 3 => {
                    (channel(values[0]) << 24)
                        | (channel(values[1]) << 16)
                        | (channel(values[2]) << 8)
                        | 0xff
                }
                "hsb" if values.len() >= 3 => {
                    // xcolor's `hsb` model (H,S,B each in [0,1]); the previous
                    // code aliased it to rgb and produced wrong colors.
                    let (r, g, b) = hsb_to_rgb(values[0], values[1], values[2]);
                    (channel(r) << 24) | (channel(g) << 16) | (channel(b) << 8) | 0xff
                }
                "gray" if !values.is_empty() => {
                    let level = channel(values[0]);
                    (level << 24) | (level << 16) | (level << 8) | 0xff
                }
                "cmyk" if values.len() >= 4 => {
                    let [c, m, y, k] = [values[0], values[1], values[2], values[3]];
                    // Standard CMYK->RGB (multiplicative), matching xcolor.
                    let red = (1.0 - c) * (1.0 - k);
                    let green = (1.0 - m) * (1.0 - k);
                    let blue = (1.0 - y) * (1.0 - k);
                    (channel(red) << 24) | (channel(green) << 16) | (channel(blue) << 8) | 0xff
                }
                _ => return None,
            };
            Some(ColorSpecial::Push(rgba))
        }
        "pop" => Some(ColorSpecial::Pop),
        _ => None,
    }
}

fn parse_classic_font_def(reader: &mut Reader<'_>, id_size: u8) -> Result<XdvFont> {
    let id = reader.sized_u32(id_size)?;
    let _checksum = reader.i32()?;
    let size = reader.u32()?;
    let design_size = reader.u32()?;
    let area_len = reader.u8()? as usize;
    let name_len = reader.u8()? as usize;
    let combined = reader.take(area_len + name_len)?;
    let name = String::from_utf8_lossy(&combined[area_len..]).to_string();
    Ok(XdvFont {
        id,
        native: false,
        name,
        size_pt: size as f64 / (1 << 20) as f64,
        design_size_pt: design_size as f64 / (1 << 20) as f64,
        face_index: 0,
        color_rgba: None,
        extend: 1.0,
        slant: 0.0,
        embolden: 0.0,
        tfm_widths: None,
    })
}

fn parse_native_font_def(reader: &mut Reader<'_>) -> Result<XdvFont> {
    let id = reader.u32()?;
    let size = reader.u32()?; // Fixed 16.16 points
    let flags = reader.u16()?;
    let name_len = reader.u8()? as usize;
    let name = String::from_utf8_lossy(reader.take(name_len)?).to_string();
    let face_index = reader.u32()?;
    let mut color_rgba = None;
    if flags & XDV_FLAG_COLORED != 0 {
        color_rgba = Some(reader.u32()?);
    }
    let mut extend = 1.0;
    let mut slant = 0.0;
    let mut embolden = 0.0;
    if flags & XDV_FLAG_EXTEND != 0 {
        extend = reader.i32()? as f64 / 65536.0;
    }
    if flags & XDV_FLAG_SLANT != 0 {
        slant = reader.i32()? as f64 / 65536.0;
    }
    if flags & XDV_FLAG_EMBOLDEN != 0 {
        embolden = reader.i32()? as f64 / 65536.0;
    }
    if flags & XDV_FLAG_VARIATIONS != 0 {
        return Err(EngineError::new(
            "XDV font variations are not supported yet",
        ));
    }
    Ok(XdvFont {
        id,
        native: true,
        name,
        size_pt: size as f64 / 65536.0,
        design_size_pt: 0.0,
        face_index,
        color_rgba,
        extend,
        slant,
        embolden,
        tfm_widths: None,
    })
}

/// A parsed `pdf:image` special: either the `bbox` form (`\includegraphics`)
/// or the `matrix` form (`\XeTeXpicfile`).
#[derive(Debug, Clone)]
struct PdfImageSpecial {
    path: String,
    /// bbox-form fields: natural size in bp plus optional explicit display
    /// width/height in points.
    bbox: Option<[f64; 4]>,
    display_width_pt: Option<f64>,
    display_height_pt: Option<f64>,
    /// matrix-form scale factors: (a, d) of the PDF matrix, i.e. the x and y
    /// scale. Kept separate so `\XeTeXpicfile ... xscaled M yscaled N` does not
    /// distort (applying `a` to both dimensions was a real bug).
    scale: Option<[f64; 2]>,
}

/// Parses `pdf:image bbox X1 Y1 X2 Y2 [clip N] [width Wpt|height Hpt] (<file>)`
/// and `pdf:image matrix A B C D E F [page N] (<file>)` specials. Returns
/// `None` for any other special.
fn parse_pdf_image_special(text: &str) -> Option<PdfImageSpecial> {
    let rest = text.trim().strip_prefix("pdf:image ")?;
    let path = rest.rfind('(').and_then(|open| {
        rest[open + 1..]
            .strip_suffix(')')
            .map(|inner| inner.trim().to_string())
    })?;
    let head = rest
        .rfind('(')
        .map(|open| rest[..open].trim())
        .unwrap_or(rest);
    let mut tokens = head.split_whitespace().peekable();
    match tokens.next()? {
        "bbox" => {
            let values: Vec<f64> = (0..4)
                .filter_map(|_| tokens.next().and_then(|t| t.parse::<f64>().ok()))
                .collect();
            if values.len() < 4 {
                return None;
            }
            let mut special = PdfImageSpecial {
                path,
                bbox: Some([values[0], values[1], values[2], values[3]]),
                display_width_pt: None,
                display_height_pt: None,
                scale: None,
            };
            while let Some(token) = tokens.next() {
                match token {
                    "width" => {
                        let value = tokens.next()?;
                        special.display_width_pt = Some(parse_pt_value(value)?);
                    }
                    "height" => {
                        let value = tokens.next()?;
                        special.display_height_pt = Some(parse_pt_value(value)?);
                    }
                    _ => {}
                }
            }
            Some(special)
        }
        "matrix" => {
            let values: Vec<f64> = (0..6)
                .filter_map(|_| tokens.next().and_then(|t| t.parse::<f64>().ok()))
                .collect();
            if values.len() < 6 {
                return None;
            }
            Some(PdfImageSpecial {
                path,
                bbox: None,
                display_width_pt: None,
                display_height_pt: None,
                scale: Some([values[0], values[3]]),
            })
        }
        _ => None,
    }
}

/// Parses `123.45pt` / `123.45bp` / bare `123.45` into f64 points.
fn parse_pt_value(token: &str) -> Option<f64> {
    let token = token.trim();
    let number = token
        .strip_suffix("pt")
        .or_else(|| token.strip_suffix("bp"))
        .unwrap_or(token);
    number.parse::<f64>().ok()
}

/// Resolves the display size in points for a parsed `pdf:image` special.
/// The bbox form scales the natural bbox by the explicit width/height when
/// present. The matrix form returns only the (x, y) scale — the renderer
/// computes the display size from the decoded image's native pixel size.
fn resolve_image_size(image: &PdfImageSpecial) -> (f64, f64, Option<[f64; 2]>) {
    if let Some([x1, y1, x2, y2]) = image.bbox {
        let natural_w = (x2 - x1).abs().max(1e-6);
        let natural_h = (y2 - y1).abs().max(1e-6);
        // graphicx may specify width, height, both, or neither; keep the
        // natural aspect ratio when only one dimension is given.
        let (w_pt, h_pt) = match (image.display_width_pt, image.display_height_pt) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, natural_h * (w / natural_w)),
            (None, Some(h)) => (natural_w * (h / natural_h), h),
            (None, None) => (natural_w, natural_h),
        };
        (w_pt, h_pt, None)
    } else if let Some(scale) = image.scale {
        (0.0, 0.0, Some(scale))
    } else {
        (100.0, 100.0, None)
    }
}

fn char_width_dvi(
    fonts: &mut HashMap<u32, XdvFont>,
    font_id: u32,
    code: u32,
    tfm_lookup: &mut TfmLookup<'_>,
) -> i64 {
    let needs_tfm = fonts
        .get(&font_id)
        .is_some_and(|font| !font.native && font.tfm_widths.is_none());
    if needs_tfm {
        let name = fonts.get(&font_id).map(|font| font.name.clone());
        let widths = name
            .as_deref()
            .and_then(tfm_lookup)
            .and_then(|bytes| parse_tfm_widths(&bytes));
        if let Some(existing) = fonts.get_mut(&font_id) {
            existing.tfm_widths = widths;
        }
    }
    let Some(font) = fonts.get(&font_id) else {
        return 0;
    };
    if font.native {
        return 0;
    }
    let Some(widths) = font.tfm_widths.as_ref() else {
        return 0;
    };
    if widths.is_empty() {
        return 0;
    }
    let index = code.min(widths.len() as u32 - 1) as usize;
    // TFM fix_word (20.12 em) scaled by the at-size (20.12 pt):
    // width in DVI units (1/65536 pt) = tfm_w * s / 2^24.
    let tfm_w = widths[index];
    let at_size = (font.size_pt * (1 << 20) as f64) as i64;
    // `at_size` saturates from a corrupt XDV font size, and `tfm_w` (a real
    // fix-word) times it can overflow i64 — which panics in debug and wraps to
    // a bogus advance in release. Saturate the product: a nonsense size yields
    // a huge advance that the glyph blit clips off-canvas, never a panic.
    (tfm_w.saturating_mul(at_size)) >> 24
}

/// Minimal TFM parser extracting per-char-code widths (fix_word, 20.12 em).
pub fn parse_tfm_widths(bytes: &[u8]) -> Option<Vec<i64>> {
    if bytes.len() < 24 {
        return None;
    }
    let u16_at = |offset: usize| -> Option<u16> {
        Some(u16::from_be_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
        ]))
    };
    let i32_at = |offset: usize| -> Option<i32> {
        Some(i32::from_be_bytes([
            *bytes.get(offset)?,
            *bytes.get(offset + 1)?,
            *bytes.get(offset + 2)?,
            *bytes.get(offset + 3)?,
        ]))
    };
    let lf = u16_at(0)? as usize;
    if bytes.len() < lf * 4 {
        return None;
    }
    let lh = u16_at(2)? as usize;
    let bc = u16_at(4)? as usize;
    let ec = u16_at(6)? as usize;
    let nw = u16_at(8)? as usize;
    if ec < bc {
        return None;
    }
    let char_info_base = 6 + lh;
    let width_base = char_info_base + (ec - bc + 1);
    if width_base + nw > lf {
        return None;
    }
    let mut widths = vec![0i64; ec - bc + 1];
    for (index, slot) in widths.iter_mut().enumerate() {
        let info_offset = (char_info_base + index) * 4;
        let width_index = *bytes.get(info_offset)? as usize;
        if width_index == 0 {
            continue;
        }
        let width = i32_at((width_base + width_index) * 4)?;
        *slot = width as i64;
    }
    Some(widths)
}

fn parse_pagesize_special(text: &str) -> Option<(f64, f64)> {
    let rest = text.strip_prefix("pdf:pagesize")?;
    let width = parse_dim_after(rest, "width")?;
    let height = parse_dim_after(rest, "height")?;
    Some((width, height))
}

fn parse_dim_after(text: &str, key: &str) -> Option<f64> {
    let position = text.find(key)? + key.len();
    let tail = text[position..].trim_start();
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(tail.len());
    tail[..end].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small builder for synthetic XDV streams in tests.
    struct Stream(Vec<u8>);

    impl Stream {
        fn new() -> Self {
            Self(Vec::new())
        }

        fn push(mut self, byte: u8) -> Self {
            self.0.push(byte);
            self
        }

        fn extend(mut self, bytes: &[u8]) -> Self {
            self.0.extend_from_slice(bytes);
            self
        }

        fn u32(mut self, value: u32) -> Self {
            self.0.extend_from_slice(&value.to_be_bytes());
            self
        }

        fn u16(mut self, value: u16) -> Self {
            self.0.extend_from_slice(&value.to_be_bytes());
            self
        }

        fn s32(mut self, value: i32) -> Self {
            self.0.extend_from_slice(&value.to_be_bytes());
            self
        }
    }

    fn no_tfm(_name: &str) -> Option<Vec<u8>> {
        None
    }

    fn synthetic_xdv() -> Vec<u8> {
        let comment = b"texpresso";
        Stream::new()
            .push(PRE)
            .push(7)
            .u32(25_400_000)
            .u32(473_628_672)
            .u32(1000)
            .push(comment.len() as u8)
            .extend(comment)
            // BOP: page 1
            .push(BOP)
            .u32(1)
            .extend(&[0u8; 36])
            .u32(0xFFFF_FFFF)
            // pdf:pagesize special
            .push(XXX1)
            .push(b"pdf:pagesize width 614.295pt height 794.97pt".len() as u8)
            .extend(b"pdf:pagesize width 614.295pt height 794.97pt")
            // Native font def: font 40, 12pt, "lmroman12-regular"
            .push(DEFINE_NATIVE_FONT)
            .u32(40)
            .u32(12 << 16) // 12.0 in 16.16 Fixed
            .u16(0)
            .push(b"lmroman12-regular".len() as u8)
            .extend(b"lmroman12-regular")
            .u32(0)
            // Select font 40 (FNT2, two-byte operand)
            .push(FNT1 + 1)
            .u16(40)
            // SET_GLYPHS: width 100, one glyph at x-offset 655360 (=10pt), y 0
            .push(SET_GLYPHS)
            .s32(100)
            .u16(1)
            .s32(655360)
            .s32(0)
            .u16(65)
            // SET_RULE 1000x1000 sp
            .push(SET_RULE)
            .s32(1000)
            .s32(1000)
            // EOP
            .push(EOP)
            // POST + POST_POST
            .push(POST)
            .extend(&[0u8; 24])
            .u16(1)
            .u16(1)
            .push(POST_POST)
            .u32(0)
            .push(7)
            .extend(&[223u8; 8])
            .0
    }

    #[test]
    fn parses_synthetic_xdv_pages_fonts_and_elements() {
        let document = parse_xdv(&synthetic_xdv(), &mut no_tfm).unwrap();
        assert_eq!(document.pages.len(), 1);
        let page = &document.pages[0];
        assert_eq!(page.page_number, 1);
        assert!((page.width_pt - 614.295).abs() < 0.01);
        assert!((page.height_pt - 794.97).abs() < 0.01);

        let font = document.fonts.get(&40).expect("native font parsed");
        assert!(font.native);
        assert_eq!(font.name, "lmroman12-regular");
        assert!((font.size_pt - 12.0).abs() < 1e-6);

        // Non-tautological pin of the DVI unit->point conversion: the header's
        // num/den (25400000 / 473628672) MUST yield exactly 1/65536 pt per unit.
        // (The rule test below previously reused `document.pt_per_unit`, so it
        // could never catch a wrong divisor -- the same class of smoke-invisible
        // risk as the classic TFM advance.)
        assert!(
            (document.pt_per_unit - (1.0 / 65536.0)).abs() < 1e-12,
            "pt_per_unit = {} (expected 1/65536)",
            document.pt_per_unit
        );

        let mut rules = 0;
        let mut glyph_runs = 0;
        for element in &page.elements {
            match element {
                XdvElement::Rule { w_pt, h_pt, .. } => {
                    rules += 1;
                    // 1000 DVI units at the pinned 1/65536 pt/unit = explicit value.
                    assert!((*w_pt - 1000.0 / 65536.0).abs() < 1e-9);
                    assert!((*h_pt - 1000.0 / 65536.0).abs() < 1e-9);
                }
                XdvElement::Glyphs {
                    font_id,
                    color_rgba,
                    glyphs,
                } => {
                    glyph_runs += 1;
                    assert_eq!(*font_id, 40);
                    assert_eq!(*color_rgba, None);
                    assert_eq!(glyphs.len(), 1);
                    assert_eq!(glyphs[0].code, 65);
                    // 655360-unit glyph offset at 1/65536 => 10pt absolute X (pen starts at 0).
                    assert!(
                        (glyphs[0].x_pt - 10.0).abs() < 1e-6,
                        "glyph x_pt = {}",
                        glyphs[0].x_pt
                    );
                }
                XdvElement::Image { .. } => {
                    panic!("synthetic XDV should not contain images");
                }
            }
        }
        assert_eq!(rules, 1);
        assert_eq!(glyph_runs, 1);
    }

    #[test]
    fn parses_synthetic_tfm_widths() {
        // header: lf=13, lh=2, bc=0, ec=2, nw=2 (rest zero)
        let mut tfm: Vec<u8> = Vec::new();
        for value in [13u16, 2, 0, 2, 2, 0, 0, 0, 0, 0, 0, 0] {
            tfm.extend_from_slice(&value.to_be_bytes());
        }
        // header_info: checksum + design size
        tfm.extend_from_slice(&[0u8; 8]);
        // char_info: 3 chars, char 0 uses width index 1
        tfm.extend_from_slice(&[1, 0, 0, 0]);
        tfm.extend_from_slice(&[0, 0, 0, 0]);
        tfm.extend_from_slice(&[0, 0, 0, 0]);
        // widths: width[0] = 0 (no glyph), width[1] = 0.75 em
        tfm.extend_from_slice(&0u32.to_be_bytes());
        tfm.extend_from_slice(&(0x000C0000u32).to_be_bytes());

        let widths = parse_tfm_widths(&tfm).expect("tfm parsed");
        assert_eq!(widths.len(), 3);
        // char 0 uses width index 1 -> slot 0 carries the 0.75em width.
        assert_eq!(widths[0], 0x000C_0000);
        assert_eq!(widths[1], 0);
    }

    #[test]
    fn char_width_dvi_saturates_on_corrupt_font_size() {
        // A corrupt XDV font size_pt makes at_size saturate to i64::MAX; a
        // real fix-word tfm_w * at_size then overflows i64 (debug panic) in
        // char_width_dvi. The saturating_mul fix must return a huge-but-finite
        // advance instead of panicking.
        let mut fonts = HashMap::new();
        fonts.insert(
            1u32,
            XdvFont {
                id: 1,
                native: false,
                name: "cmr10".to_string(),
                size_pt: 1e15,
                design_size_pt: 10.0,
                face_index: 0,
                color_rgba: None,
                extend: 1.0,
                slant: 0.0,
                embolden: 0.0,
                tfm_widths: Some(vec![1_000_000i64; 400]),
            },
        );
        let mut tfm = |_name: &str| -> Option<Vec<u8>> { None };
        let w = char_width_dvi(&mut fonts, 1, 10, &mut tfm);
        assert!(
            w > 0,
            "huge but finite saturated advance, no overflow panic"
        );
    }

    #[test]
    fn corrupt_tfm_bytes_degrade_without_panicking() {
        // parse_tfm_widths is fed real .tfm bytes resolved from disk / kpsewhich;
        // a corrupt or hand-built TFM must return None, never panic on the
        // length / offset / ec<bc-underflow guards.
        fn header(lf: u16, lh: u16, bc: u16, ec: u16, nw: u16) -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(&lf.to_be_bytes());
            v.extend_from_slice(&lh.to_be_bytes());
            v.extend_from_slice(&bc.to_be_bytes());
            v.extend_from_slice(&ec.to_be_bytes());
            v.extend_from_slice(&nw.to_be_bytes());
            v.resize(24, 0);
            v
        }
        assert_eq!(parse_tfm_widths(&[]), None); // < 24 bytes
        assert_eq!(parse_tfm_widths(&[0u8; 10]), None);
        assert!(parse_tfm_widths(&header(6, 0, 5, 2, 1)).is_none()); // ec < bc
        assert!(parse_tfm_widths(&header(6, 0, 0, 0, 100)).is_none()); // width_base + nw > lf
        assert!(parse_tfm_widths(&header(65535, 65535, 0, 65535, 65535)).is_none()); // lf beyond buf
        assert!(parse_tfm_widths(&vec![0xABu8; 64]).is_none()); // garbage
    }

    #[test]
    fn classic_tfm_advances_match_tftopl_reference() {
        // Authoritative regression for `parse_tfm_widths` + the `char_width_dvi`
        // TFM->DVI advance: the per-code classic-font advance at 10pt for the real
        // cmr10.tfm must match widths read independently from `tftopl`
        // (em x design-size 10pt x 65536). This is the glyph spacing the
        // dark-pixel render smoke cannot see, so it is pinned here. Self-skips
        // when no TeX distribution is present (CI-safe).
        let out = match std::process::Command::new("kpsewhich")
            .arg("cmr10.tfm")
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return,
        };
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let widths = parse_tfm_widths(&bytes).expect("real cmr10.tfm parses");
        let dvi_at_10pt =
            |code: usize| -> i64 { (widths[code] * (10.0f64 * (1i64 << 20) as f64) as i64) >> 24 };
        // code -> expected DVI units (tftopl em * 10 * 65536), +/-2 for rounding.
        for (code, expected) in [
            (32u32, 182045i64), // space (kern-tagged)
            (45, 218454),       // hyphen (kern-tagged)
            (65, 491521),       // A
            (77, 600748),       // M
            (105, 182045),      // i
            (108, 182045),      // l
            (120, 345887),      // x
        ] {
            let got = dvi_at_10pt(code as usize);
            assert!(
                (got - expected).abs() <= 2,
                "cmr10 code {code}: got {got}, tftopl reference {expected}"
            );
        }
    }

    #[test]
    fn parses_real_xdv_when_env_var_points_at_artifact() {
        let Ok(path) = std::env::var("OXIPRESSO_XDV_SMOKE") else {
            return;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let document = parse_xdv(&bytes, &mut no_tfm).unwrap();
        assert!(
            !document.pages.is_empty(),
            "real XDV should contain at least one page"
        );
        assert!(
            document
                .fonts
                .values()
                .any(|font| font.native && !font.name.is_empty()),
            "real XDV should contain native font definitions"
        );
        // Page digests must be computable and unique enough for the
        // incremental renderer on real documents.
        let mut digests: Vec<u64> = (0..document.pages.len())
            .map(|index| document.page_digest(index).expect("page digest"))
            .collect();
        let unique = digests.len();
        digests.sort_unstable();
        digests.dedup();
        assert_eq!(
            digests.len(),
            unique,
            "real XDV pages should have unique digests"
        );
    }

    #[test]
    fn decodes_hsb_color_special_not_rgb() {
        // xcolor hsb: (0,1,1)=red, (2/3,1,1)=blue, (0,0,0.5)=50% gray.
        // The old code treated hsb as raw rgb, so (0,1,1) would have been
        // near-black, not red.
        let red = parse_color_special("color push hsb 0 1 1");
        assert!(
            matches!(red, Some(ColorSpecial::Push(0xff00_00ff))),
            "hsb red"
        );
        let blue = parse_color_special("color push hsb 0.66666666 1 1");
        assert!(
            matches!(blue, Some(ColorSpecial::Push(0x0000_ffff))),
            "hsb blue"
        );
        let gray = parse_color_special("color push hsb 0 0 0.5");
        assert!(
            matches!(gray, Some(ColorSpecial::Push(0x8080_80ff))),
            "hsb gray"
        );
    }

    #[test]
    fn converts_cmyk_color_special_multiplicatively() {
        // Standard CMYK->RGB (1-c)(1-k): primaries and a mid-tone that the
        // previous additive formula got wrong.
        let cyan = parse_color_special("color push cmyk 1 0 0 0");
        assert!(matches!(cyan, Some(ColorSpecial::Push(0x00ff_ffff))));
        let white = parse_color_special("color push cmyk 0 0 0 0");
        assert!(matches!(white, Some(ColorSpecial::Push(0xffff_ffff))));
        let black = parse_color_special("color push cmyk 0 0 0 1");
        assert!(matches!(black, Some(ColorSpecial::Push(0x0000_00ff))));
        // cmyk(0.5,0,0,0.5): R=(0.5)(0.5)=0.25->64, G=B=(1)(0.5)=0.5->128.
        let teal = parse_color_special("color push cmyk 0.5 0 0 0.5");
        assert!(
            matches!(teal, Some(ColorSpecial::Push(0x4080_80ff))),
            "mid-tone cmyk must use multiplicative conversion"
        );
    }

    #[test]
    fn parses_color_specials_into_colored_glyph_runs() {
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&[247u8, 7]);
        stream.extend_from_slice(&25_400_000u32.to_be_bytes());
        stream.extend_from_slice(&473_628_672u32.to_be_bytes());
        stream.extend_from_slice(&1000u32.to_be_bytes());
        stream.push(0);
        stream.extend_from_slice(&[139u8]); // BOP
        stream.extend_from_slice(&[0u8; 40]);
        stream.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        stream.extend_from_slice(&[252u8]); // native font def, id 7
        stream.extend_from_slice(&7u32.to_be_bytes());
        stream.extend_from_slice(&(12u32 << 16).to_be_bytes());
        stream.extend_from_slice(&0u16.to_be_bytes());
        stream.push(5);
        stream.extend_from_slice(b"times");
        stream.extend_from_slice(&0u32.to_be_bytes());
        stream.extend_from_slice(&[236u8]); // FNT2 -> font 7
        stream.extend_from_slice(&7u16.to_be_bytes());
        // color push rgb 1 0 0 (red)
        let red_special = b"color push rgb 1 0 0";
        stream.extend_from_slice(&[239u8, red_special.len() as u8]);
        stream.extend_from_slice(red_special);
        stream.extend_from_slice(&[253u8]); // SET_GLYPHS: red 'A'
        stream.extend_from_slice(&50i32.to_be_bytes());
        stream.extend_from_slice(&1u16.to_be_bytes());
        stream.extend_from_slice(&0i32.to_be_bytes());
        stream.extend_from_slice(&0i32.to_be_bytes());
        stream.extend_from_slice(&65u16.to_be_bytes());
        // color pop -> back to black
        let pop_special = b"color pop";
        stream.extend_from_slice(&[239u8, pop_special.len() as u8]);
        stream.extend_from_slice(pop_special);
        stream.extend_from_slice(&[253u8]); // SET_GLYPHS: black 'B'
        stream.extend_from_slice(&50i32.to_be_bytes());
        stream.extend_from_slice(&1u16.to_be_bytes());
        stream.extend_from_slice(&0i32.to_be_bytes());
        stream.extend_from_slice(&0i32.to_be_bytes());
        stream.extend_from_slice(&66u16.to_be_bytes());
        stream.extend_from_slice(&[140u8]); // EOP

        let document = parse_xdv(&stream, &mut no_tfm).unwrap();
        let page = &document.pages[0];
        let mut colors: Vec<Option<u32>> = Vec::new();
        for element in &page.elements {
            if let XdvElement::Glyphs {
                color_rgba, glyphs, ..
            } = element
            {
                assert_eq!(glyphs.len(), 1);
                colors.push(*color_rgba);
            }
        }
        assert_eq!(
            colors,
            vec![Some(0xFF0000FF), None],
            "red run then default run"
        );
        // Digests must reflect the color difference.
        assert_ne!(document.page_digest(0), {
            let mut without_colors = document.clone();
            for element in &mut without_colors.pages[0].elements {
                if let XdvElement::Glyphs { color_rgba, .. } = element {
                    *color_rgba = None;
                }
            }
            without_colors.page_digest(0)
        });
    }

    #[test]
    fn parses_pdf_image_specials_from_real_engine_layout() {
        // Modeled byte-for-byte on the real engine's includegraphics output:
        // an \includegraphics bbox special and an \XeTeXpicfile matrix special.
        let bbox_text =
            b"pdf:image bbox 0 0 511.99873 511.99873 clip 0 width 256.95935pt (../doc/logo.png)";
        let matrix_text =
            b"pdf:image matrix 0.27682 0.0 0.0 0.27682 0.0 0.0 page 0 (../doc/logo.png)";

        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&[247u8, 7]);
        stream.extend_from_slice(&25_400_000u32.to_be_bytes());
        stream.extend_from_slice(&473_628_672u32.to_be_bytes());
        stream.extend_from_slice(&1000u32.to_be_bytes());
        stream.push(0);
        stream.extend_from_slice(&[139u8]); // BOP
        stream.extend_from_slice(&[0u8; 40]);
        stream.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());

        stream.extend_from_slice(&[239u8, bbox_text.len() as u8]);
        stream.extend_from_slice(bbox_text);

        stream.extend_from_slice(&[DOWN4]); // move down, then the matrix image
        stream.extend_from_slice(&1_000_000i32.to_be_bytes());
        stream.extend_from_slice(&[239u8, matrix_text.len() as u8]);
        stream.extend_from_slice(matrix_text);

        stream.extend_from_slice(&[140u8]); // EOP

        let document = parse_xdv(&stream, &mut no_tfm).unwrap();
        let page = &document.pages[0];
        let mut images = page
            .elements
            .iter()
            .filter_map(|element| match element {
                XdvElement::Image {
                    x_pt,
                    y_pt,
                    w_pt,
                    h_pt,
                    scale,
                    path,
                } => Some((*x_pt, *y_pt, *w_pt, *h_pt, *scale, path.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(images.len(), 2, "both pdf:image specials parse");

        let (x, y, w, h, scale, path) = images.remove(0);
        assert!((w - 256.95935).abs() < 1e-3, "bbox width, got {w}");
        assert!((h - 256.95935).abs() < 1e-3, "square bbox keeps aspect");
        assert_eq!(scale, None);
        assert_eq!(path, "../doc/logo.png");
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);

        let (_x, y, w, h, scale, path) = images.remove(0);
        assert_eq!(scale, Some([0.27682, 0.27682]));
        assert_eq!(w, 0.0, "matrix form defers sizing to the renderer");
        assert_eq!(h, 0.0);
        assert_eq!(path, "../doc/logo.png");
        // DOWN4 by 1_000_000 sp = 1_000_000/65536 pt.
        assert!(
            (y - 15.258_789).abs() < 1e-4,
            "image placed at pen y, got {y}"
        );
        // Digest must include images: removing them changes the digest.
        let digest_with = document.page_digest(0).unwrap();
        let mut without = document.clone();
        without.pages[0]
            .elements
            .retain(|element| !matches!(element, XdvElement::Image { .. }));
        assert_ne!(digest_with, without.page_digest(0).unwrap());
    }

    #[test]
    fn parses_nonuniform_matrix_image_xy_scale() {
        // `\XeTeXpicfile ... xscaled M yscaled N` => matrix with a != d. The
        // parser must keep both axes separate (using `a` for both distorted the
        // image) — here a=2, d=4.
        let special = parse_pdf_image_special("pdf:image matrix 2 0 0 4 0 0 page 0 (a.png)")
            .expect("matrix special parses");
        assert_eq!(
            special.scale,
            Some([2.0, 4.0]),
            "x and y scale kept separate"
        );
        assert_eq!(special.bbox, None);
    }

    /// Builds a one-page XDV containing a pagesize + one `pdf:image` special.
    fn xdv_with_image_special(special: &str) -> Vec<u8> {
        let pagesize = b"pdf:pagesize width 614.295pt height 794.96999pt";
        let mut stream: Vec<u8> = Vec::new();
        stream.extend_from_slice(&[247u8, 7]);
        stream.extend_from_slice(&25_400_000u32.to_be_bytes());
        stream.extend_from_slice(&473_628_672u32.to_be_bytes());
        stream.extend_from_slice(&1000u32.to_be_bytes());
        stream.push(0);
        stream.extend_from_slice(&[139u8]);
        stream.extend_from_slice(&[0u8; 40]);
        stream.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        stream.extend_from_slice(&[239u8, pagesize.len() as u8]);
        stream.extend_from_slice(pagesize);
        let s = special.as_bytes();
        stream.extend_from_slice(&[239u8, s.len() as u8]);
        stream.extend_from_slice(s);
        stream.extend_from_slice(&[140u8]);
        stream
    }

    fn first_image_size(special: &str) -> (f64, f64) {
        let doc = parse_xdv(&xdv_with_image_special(special), &mut no_tfm).unwrap();
        doc.pages[0]
            .elements
            .iter()
            .find_map(|element| match element {
                XdvElement::Image { w_pt, h_pt, .. } => Some((*w_pt, *h_pt)),
                _ => None,
            })
            .expect("image element")
    }

    #[test]
    fn image_bbox_aspect_ratio_honors_width_height_or_both() {
        // natural bbox 100x50 (2:1). width-only scales height to keep aspect;
        // height-only must scale width to keep aspect (was a real bug);
        // both honored independently; neither uses natural size.
        let (w, h) = first_image_size("pdf:image bbox 0 0 100 50 clip 0 width 40pt (x.png)");
        assert!(
            (w - 40.0).abs() < 1e-3 && (h - 20.0).abs() < 1e-3,
            "width-only -> 40x20"
        );

        let (w, h) = first_image_size("pdf:image bbox 0 0 100 50 clip 0 height 25pt (x.png)");
        assert!(
            (w - 50.0).abs() < 1e-3 && (h - 25.0).abs() < 1e-3,
            "height-only must scale width to keep 2:1 aspect, got {w}x{h}"
        );

        let (w, h) =
            first_image_size("pdf:image bbox 0 0 100 50 clip 0 width 30pt height 10pt (x.png)");
        assert!(
            (w - 30.0).abs() < 1e-3 && (h - 10.0).abs() < 1e-3,
            "both honored"
        );

        let (w, h) = first_image_size("pdf:image bbox 0 0 100 50 clip 0 (x.png)");
        assert!(
            (w - 100.0).abs() < 1e-3 && (h - 50.0).abs() < 1e-3,
            "neither -> natural"
        );
    }

    #[test]
    fn page_digest_is_stable_and_content_sensitive() {
        let document = parse_xdv(&synthetic_xdv(), &mut no_tfm).unwrap();
        let digest = document.page_digest(0).expect("page digest");
        assert_eq!(digest, document.page_digest(0).expect("page digest"));

        // Parsing the same bytes again must produce the same digest.
        let reparsed = parse_xdv(&synthetic_xdv(), &mut no_tfm).unwrap();
        assert_eq!(digest, reparsed.page_digest(0).expect("page digest"));

        // Moving a glyph changes the digest.
        let mut shifted = document.clone();
        for element in &mut shifted.pages[0].elements {
            if let XdvElement::Glyphs { glyphs, .. } = element {
                glyphs[0].x_pt += 1.0;
            }
        }
        assert_ne!(digest, shifted.page_digest(0).expect("page digest"));

        // Adding an element changes the digest.
        let mut with_rule = document.clone();
        with_rule.pages[0].elements.push(XdvElement::Rule {
            x_pt: 0.0,
            y_pt: 0.0,
            w_pt: 1.0,
            h_pt: 1.0,
        });
        assert_ne!(digest, with_rule.page_digest(0).expect("page digest"));
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct State {
    h: i64,
    v: i64,
    w: i64,
    x: i64,
    y: i64,
    z: i64,
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    last_width: i64,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| EngineError::new("unexpected end of DVI/XDV stream"))?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&[u8]> {
        let end = self.pos + len;
        if end > self.bytes.len() {
            return Err(EngineError::new("unexpected end of DVI/XDV stream"));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }

    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    /// Unsigned operand of `size` bytes (1-4), big-endian.
    fn sized_u32(&mut self, size: u8) -> Result<u32> {
        let mut value = 0u32;
        for _ in 0..size {
            value = (value << 8) | self.u8()? as u32;
        }
        Ok(value)
    }

    /// Signed operand of `size` bytes (1-4), big-endian two's complement.
    fn sized_i32(&mut self, size: u8) -> Result<i32> {
        let raw = self.sized_u32(size)?;
        if size == 4 {
            return Ok(raw as i32);
        }
        let bits = size * 8;
        let sign_shift = 32 - bits;
        Ok(((raw << sign_shift) as i32) >> sign_shift)
    }
}
