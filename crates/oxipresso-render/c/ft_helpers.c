/* Direct field accessors for FreeType faces/slots.
 *
 * oxipresso-render's Rust side is layout-independent (FT_Pos is 4 bytes on
 * Windows MSVC and 8 elsewhere, which shifts every later field), so instead
 * of scanning face memory for back-referencing pointers - a heap-layout
 * dependent heuristic that can pick a wrong candidate and crash - these
 * accessors read the fields through the real C headers.
 */
#include <ft2build.h>
#include FT_FREETYPE_H

void *oxipresso_ft_face_glyph_slot(FT_Face face) {
    return face ? (void *)face->glyph : NULL;
}

FT_Bitmap *oxipresso_ft_slot_bitmap(FT_GlyphSlot slot) {
    return slot ? &slot->bitmap : NULL;
}

int oxipresso_ft_slot_bitmap_left(FT_GlyphSlot slot) {
    return slot ? slot->bitmap_left : 0;
}

int oxipresso_ft_slot_bitmap_top(FT_GlyphSlot slot) {
    return slot ? slot->bitmap_top : 0;
}
