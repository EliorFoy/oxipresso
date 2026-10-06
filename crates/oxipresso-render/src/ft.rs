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
pub type FT_Face = *mut c_void;
pub type FT_GlyphSlot = *mut c_void;

pub const FT_LOAD_DEFAULT: c_int = 0;
pub const FT_LOAD_NO_BITMAP: c_int = 0x8;
pub const FT_LOAD_NO_HINTING: c_int = 0x2;
pub const FT_RENDER_MODE_NORMAL: c_uint = 0;
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

pub const FT_ENCODING_ADOBE_CUSTOM: u32 = 0x41_44_43_55; // 'ADCU'

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
    pub fn FT_Get_Name_Index(face: FT_Face, glyph_name: *const i8) -> u32;
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
pub fn find_glyph_slot(face: FT_Face) -> Option<FT_GlyphSlot> {
    const SCAN_RANGE: usize = 232;
    unsafe {
        let mut values: Vec<usize> = Vec::new();
        for offset in (0usize..SCAN_RANGE).step_by(8) {
            let value = (face.cast::<u8>().add(offset) as *const usize).read();
            values.push(value);
        }
        // Heap-cluster anchor: pointer-like values cluster within a few
        // megabytes of each other; numeric fields (flags, glyph counts, font
        // coordinates) do not.
        let pointer_like: Vec<usize> = values
            .iter()
            .copied()
            .filter(|value| *value > 0x10000 && value % 8 == 0)
            .collect();
        let near = |candidate: usize| -> bool {
            pointer_like
                .iter()
                .filter(|other| {
                    let distance = candidate.abs_diff(**other);
                    distance > 0 && distance < 0x1000_0000
                })
                .count()
                >= 2
        };
        for (index, &candidate) in values.iter().enumerate() {
            let offset = index * 8;
            if candidate <= 0x10000 || offset % 8 != 0 || !near(candidate) {
                continue;
            }
            let slot = candidate as *mut c_void;
            let back_reference = read_pointer(slot, SLOT_FACE_OFFSET);
            if back_reference == face {
                #[cfg(test)]
                eprintln!("[dbg] slot candidate at face+{offset}");
                return Some(slot);
            }
        }
    }
    None
}

/// Copies out the rendered gray bitmap plus its bearing relative to the pen
/// position. The `FT_Bitmap` offset inside `FT_GlyphSlotRec` varies across
/// FreeType versions (2.14 added `glyph_index` + `generic` fields), so the
/// struct is located by signature: positive rows/width, `|pitch| >= width`,
/// a non-null buffer, and the gray pixel-mode byte — all validated together.
/// The scan steps by 4 because the bitmap may sit at a 4-aligned but not
/// 8-aligned offset.
pub fn read_rendered_bitmap(slot: FT_GlyphSlot) -> Option<RenderedGrayBitmap> {
    unsafe {
        for base_offset in (0usize..1024).step_by(4) {
            let base = slot.cast::<u8>().add(base_offset);
            let rows = (base.add(BITMAP_ROWS_OFFSET) as *const c_uint).read();
            let width = (base.add(BITMAP_WIDTH_OFFSET) as *const c_uint).read();
            let pitch = (base.add(BITMAP_PITCH_OFFSET) as *const c_int).read();
            let buffer = (base.add(BITMAP_BUFFER_OFFSET) as *const *mut u8).read();
            let pixel_mode = base.add(BITMAP_PIXEL_MODE_OFFSET).read();
            #[cfg(test)]
            if rows > 0 && rows < 8192 {
                eprintln!(
                    "[dbg] scan@{base_offset}: rows={rows} width={width} pitch={pitch} mode={pixel_mode} buffer_null={}",
                    buffer.is_null()
                );
            }
            if rows == 0 || width == 0 || rows > 8192 || width > 8192 {
                continue;
            }
            if (pitch.unsigned_abs() as u32) < width {
                continue;
            }
            if buffer.is_null() || pixel_mode != FT_PIXEL_MODE_GRAY {
                continue;
            }
            let left = (base.add(40) as *const c_int).read();
            let top = (base.add(44) as *const c_int).read();
            if left.abs() > 100_000 || top.abs() > 100_000 {
                continue;
            }
            let width = width as usize;
            let height = rows as usize;
            let stride = pitch.unsigned_abs() as usize;
            let mut pixels = Vec::with_capacity(width * height);
            for row in 0..height {
                pixels
                    .extend_from_slice(std::slice::from_raw_parts(buffer.add(row * stride), width));
            }
            return Some(RenderedGrayBitmap {
                left: left as i64,
                top: top as i64,
                width: width as u32,
                height: height as u32,
                pixels,
            });
        }
    }
    None
}
