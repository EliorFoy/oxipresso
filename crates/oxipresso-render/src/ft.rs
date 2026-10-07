//! Minimal hand-written FreeType bindings for glyph rasterization.
//!
//! Only the handful of functions needed to rasterize a glyph bitmap from an
//! in-memory font file are bound. Instead of relying on a specific
//! `FT_FaceRec` layout (which changed across FreeType versions), the glyph
//! slot pointer is discovered at runtime: `FT_GlyphSlotRec.face` points back
//! to the owning face, so the slot is the heap pointer whose `face` field
//! (second qword) matches the face pointer.

#![allow(non_camel_case_types, dead_code)]

use std::os::raw::{c_int, c_long, c_uint, c_void};

pub type FT_Library = *mut c_void;
pub type FT_CharMap = *mut c_void;
pub type FT_Face = *mut c_void;
pub type FT_GlyphSlot = *mut c_void;

pub const FT_LOAD_DEFAULT: c_int = 0;
pub const FT_LOAD_NO_BITMAP: c_int = 0x8;
pub const FT_LOAD_NO_HINTING: c_int = 0x2;
/// FT_LOAD_TARGET_(FT_RENDER_MODE_LIGHT): vertical-only hinting — sharp
/// stems at text sizes without the full autohinter's cost or distortion.
pub const FT_LOAD_TARGET_LIGHT: c_int = 0x1_0000;
pub const FT_RENDER_MODE_NORMAL: c_uint = 0;
pub const FT_PIXEL_MODE_MONO: u8 = 1;
pub const FT_PIXEL_MODE_GRAY: u8 = 2;

/// 16.16 fixed-point transformation matrix. FreeType applies
/// `x' = xx*x + xy*y`, `y' = yx*x + yy*y` to the glyph outline before
/// rasterization; used for XDV slant/extend font transforms.
/// NOTE: FT_Set_Transform is NOT used because MSVC's FT_Pos (long) is 4
/// bytes, invalidating the struct offsets we rely on. Slant/extend are
/// applied as bitmap post-processing instead.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FT_Matrix {
    pub xx: i64,
    pub xy: i64,
    pub yx: i64,
    pub yy: i64,
}

/// Offsets into `FT_GlyphSlotRec` that have been stable across FreeType 2.x:
/// `face` at 8. The `FT_Bitmap` is located by signature scan instead, because
/// its offset shifted when FreeType restructured `FT_GlyphSlotRec`.
pub const SLOT_FACE_OFFSET: usize = 8;

pub const BITMAP_ROWS_OFFSET: usize = 0;
pub const BITMAP_WIDTH_OFFSET: usize = 4;
pub const BITMAP_PITCH_OFFSET: usize = 8;
pub const BITMAP_BUFFER_OFFSET: usize = 16;
pub const BITMAP_PIXEL_MODE_OFFSET: usize = 26;

/// A rendered gray bitmap with its bearing relative to the pen position
/// (FreeType semantics: `left` is right of the pen, `top` is above it).
pub struct RenderedGrayBitmap {
    pub left: i64,
    pub top: i64,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub const FT_ENCODING_ADOBE_CUSTOM: u32 = 0x41444243; // FT_ENC_TAG A D B C
pub const FT_ENCODING_ADOBE_STANDARD: u32 = 0x41444F42; // FT_ENC_TAG A D O B
pub const FT_ENCODING_MS_SYMBOL: u32 = 0x73796D62; // FT_ENC_TAG s y m b

unsafe extern "C" {
    pub fn FT_Init_FreeType(library: *mut FT_Library) -> c_int;
    pub fn FT_Done_FreeType(library: FT_Library) -> c_int;
    pub fn FT_New_Memory_Face(
        library: FT_Library,
        data: *const u8,
        size: c_long,
        face_index: c_long,
        face: *mut FT_Face,
    ) -> c_int;
    pub fn FT_Done_Face(face: FT_Face) -> c_int;
    pub fn FT_Set_Pixel_Sizes(face: FT_Face, width: c_uint, height: c_uint) -> c_int;
    pub fn FT_Get_Char_Index(face: FT_Face, charcode: c_uint) -> c_uint;
    pub fn FT_Select_Charmap(face: FT_Face, encoding: c_uint) -> c_int;
    pub fn FT_Get_Glyph_Name(glyph_index: c_uint, buffer: *mut u8, buffer_max: c_uint) -> c_int;
    pub fn FT_Get_Name_Index(face: FT_Face, glyph_name: *const i8) -> u32;
    pub fn oxipresso_ft_face_glyph_slot(face: FT_Face) -> *mut c_void;
    pub fn oxipresso_ft_slot_bitmap(slot: *mut c_void) -> *mut c_void;
    pub fn oxipresso_ft_slot_bitmap_left(slot: *mut c_void) -> c_int;
    pub fn oxipresso_ft_slot_bitmap_top(slot: *mut c_void) -> c_int;
    pub fn FT_Load_Glyph(face: FT_Face, glyph_index: c_uint, load_flags: c_int) -> c_int;
    pub fn FT_Set_Transform(face: FT_Face, matrix: *const FT_Matrix, delta: *const c_void)
    -> c_int;
    pub fn FT_Render_Glyph(slot: FT_GlyphSlot, render_mode: c_uint) -> c_int;
}

fn read_pointer(base: *const c_void, offset: usize) -> *mut c_void {
    unsafe { (base.cast::<u8>().add(offset) as *const *mut c_void).read() }
}

/// Finds the face's glyph slot by locating a pointer field whose second qword
/// points back at the face (FT_GlyphSlotRec.face). Only called after a glyph
/// load, when `face->glyph` is guaranteed to be set.
///
/// To avoid dereferencing data bytes that merely look like pointers (for
/// example `FT_BBox` coordinates), candidates must first pass a heap-cluster
/// filter: their address must be near other pointer values found in the face
/// structure, which all come from the same allocator region.
/// Returns the face's glyph slot through the C accessor (compiled against
/// the real FreeType headers — no layout guessing, no memory scanning).
pub fn find_glyph_slot(face: FT_Face) -> Option<FT_GlyphSlot> {
    unsafe {
        let slot = oxipresso_ft_face_glyph_slot(face);
        if slot.is_null() { None } else { Some(slot) }
    }
}

/// Copies out the rendered gray bitmap plus its bearing relative to the pen
/// position. The bitmap fields are read through the C accessor for the slot
/// and fixed offsets inside `FT_Bitmap` itself (rows/width/pitch/buffer have
/// no `FT_Pos` and are stable across platforms).
pub fn read_rendered_bitmap(slot: FT_GlyphSlot) -> Option<RenderedGrayBitmap> {
    unsafe {
        let bitmap = oxipresso_ft_slot_bitmap(slot);
        if bitmap.is_null() {
            return None;
        }
        let base = bitmap.cast::<u8>();
        let rows = (base.cast::<u32>()).read();
        let width = (base.add(4).cast::<u32>()).read();
        let pitch = (base.add(8).cast::<i32>()).read();
        let buffer = (base.add(16).cast::<*const u8>()).read();
        // FT_Bitmap has no FT_Pos fields: rows/width/pitch/buffer are at
        // fixed offsets, and pixel_mode sits at 26 (BITMAP_PIXEL_MODE_OFFSET).
        let pixel_mode = (base.add(26).cast::<u8>()).read();
        if rows == 0 || width == 0 || buffer.is_null() || pitch == 0 {
            return None;
        }
        let abs_pitch = pitch.unsigned_abs() as usize;
        let mut pixels = Vec::with_capacity(rows as usize * width as usize);
        // CJK system fonts (SimSun.ttc) carry MONO (1bpp) embedded strikes;
        // at a pixel size matching a strike FT_Load_Glyph returns MONO, not
        // GRAY. Expand MSB-first 1bpp rows to 8-bit gray.
        if pixel_mode == FT_PIXEL_MODE_MONO {
            for row in 0..rows as usize {
                let row_index = if pitch > 0 {
                    row
                } else {
                    rows as usize - 1 - row
                };
                let line = std::slice::from_raw_parts(
                    buffer.add(row_index * abs_pitch),
                    (width as usize).div_ceil(8),
                );
                for x in 0..width as usize {
                    let bit = (line[x / 8] >> (7 - (x % 8))) & 1;
                    pixels.push(if bit == 1 { 255 } else { 0 });
                }
            }
        } else if pixel_mode == FT_PIXEL_MODE_GRAY {
            for row in 0..rows as usize {
                let row_index = if pitch > 0 {
                    row
                } else {
                    rows as usize - 1 - row
                };
                let source = buffer.add(row_index * abs_pitch);
                let line = std::slice::from_raw_parts(source, width as usize);
                pixels.extend_from_slice(line);
            }
        } else {
            return None;
        }
        let left = oxipresso_ft_slot_bitmap_left(slot) as i64;
        let top = oxipresso_ft_slot_bitmap_top(slot) as i64;
        Some(RenderedGrayBitmap {
            width,
            height: rows,
            left,
            top,
            pixels,
        })
    }
}
