# gpui-pre-windows 0.3.12, patched

Copy of [gpui-pre-windows 0.3.12](https://crates.io/crates/gpui-pre-windows)
(Zed's `gpui_windows` crate, Apache-2.0) with two local patches.

## Right-to-left text, `src/direct_write.rs`

Right-to-left text (Arabic, Hebrew, ...) was drawn mirrored.

- `DrawGlyphRun`: each run is placed at the origin DirectWrite passes in, and the
  glyphs of a right-to-left run (odd `bidiLevel`) go from that origin leftwards.
  Upstream laid every run out left to right after the previous one, but DirectWrite
  hands runs over in text order and right-to-left runs list their glyphs in reading
  order.
- Tabs are placed where the layout's hit test puts them, for the same reason.
- `StringIndexConverter::advance_to_utf16_ix` can move backwards.
- Tests: `rtl_runs_are_laid_out_right_to_left` is new; the caret checks in
  `tabs_preserve_native_advances_and_cursor` skip right-to-left strings, since they
  assumed the old left-to-right layout.

## Reuse of freed atlas textures, `src/directx_atlas.rs`

An image larger than a 1024x1024 atlas page gets a texture of its own. The video
player hands GPUI a new image every frame and drops the previous one, so upstream
created a full-size texture and shader resource view per frame and released one.

- When the last tile of a texture is removed, the texture (with its view) goes to
  `spare_textures` instead of being released. At most `MAX_SPARE_TEXTURES` (2) are
  kept; the oldest is released when a third arrives.
- `push_texture` takes a spare texture of exactly the same size and kind before
  creating a new one, and clears its allocator. Texture and view creation moved
  unchanged into `create_texture`.
- `handle_device_lost` drops the spare textures with the rest.
- Test: `test_freed_texture_is_reused_for_same_size` is new.

Every change is marked with "Stream Tube patch". Drop this folder and the
`[patch.crates-io]` entry in `Cargo.toml` once upstream handles bidi text and
reuses freed textures (or the player stops uploading frames as images).
