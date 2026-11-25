# gpui-pre-wgpu 0.3.12, patched

Copy of [gpui-pre-wgpu 0.3.12](https://crates.io/crates/gpui-pre-wgpu)
(Zed's `gpui_wgpu` crate, Apache-2.0, the renderer GPUI uses on Linux), unpacked
unchanged from the published `.crate` (sha256 `dc76608d…caf852`, the checksum
`Cargo.lock` had), with one local patch in `src/wgpu_atlas.rs`. The video player
hands GPUI a new image (about 8 MB at 1080p) every frame, and upstream copied
each one an extra time and created a texture for it.

- No extra copy: `upload_texture` used to copy every upload into a `Vec` for
  `before_frame` to write later, swizzled or not. When the bytes are already in
  the texture's format (every `R8Unorm` and `Bgra8Unorm` texture; only an
  `Rgba8Unorm` atlas swizzles) it now calls `queue.write_texture` right away.
  The bytes are only borrowed for the call (`AtlasBackend::insert` in gpui-pre
  takes `&[u8]`), so they cannot be kept for later without a copy. wgpu stages
  its own copy and applies it before the next submit, as it did when the write
  came from `before_frame`. A texture's uploads all take the same path, so they
  stay in order. The `write_texture` call moved unchanged into `write_tile`.
- Texture reuse: when the last tile of a texture is removed, the texture (with
  its view) goes to `spare_textures` instead of being released, at most
  `MAX_SPARE_TEXTURES` (2), oldest released first. `push_texture` takes a spare
  of exactly the same size, kind and format before creating one, clears its
  allocator and gives it a new generation, so the renderer rebuilds its bind
  group. Texture and view creation moved unchanged into `create_texture`.
  `clear` and `handle_device_lost` drop the spares.
- Tests: `only_swizzled_uploads_are_deferred` and
  `freed_texture_is_reused_for_same_size` are new. The crate's tests do not
  build from the published crate alone (they need `naga` as a dev-dependency and
  Zed's font assets), and CI does not run them.

Every change is marked with "Stream Tube patch". Drop this folder and its
`[patch.crates-io]` entry in `Cargo.toml` once upstream uploads without the copy
and reuses freed textures (or the player stops uploading frames as images).
