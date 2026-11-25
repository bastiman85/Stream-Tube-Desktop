// Modified for gpui-pre (snapshot of zed@d2a4fb6): adapt device queues and sRGB detection to wgpu 30.0.1.
use anyhow::{Context as _, Result};
use etagere::{BucketedAtlasAllocator, size2};
use gpui::{
    AtlasBackend, AtlasKey, AtlasState, AtlasTextureId, AtlasTextureKind, AtlasTextureList,
    AtlasTile, Bounds, DevicePixels, PlatformAtlas, Point, Size,
};
use parking_lot::Mutex;
use std::{borrow::Cow, ops, sync::Arc};

use crate::WgpuContext;

fn device_size_to_etagere(size: Size<DevicePixels>) -> etagere::Size {
    size2(size.width.0, size.height.0)
}

fn etagere_point_to_device(point: etagere::Point) -> Point<DevicePixels> {
    Point {
        x: DevicePixels(point.x),
        y: DevicePixels(point.y),
    }
}

pub struct WgpuAtlas(Mutex<AtlasState<WgpuAtlasTextures>>);

struct PendingUpload {
    id: AtlasTextureId,
    bounds: Bounds<DevicePixels>,
    data: Vec<u8>,
}

struct WgpuAtlasTextures {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    max_texture_size: u32,
    color_texture_format: wgpu::TextureFormat,
    storage: WgpuAtlasStorage,
    pending_uploads: Vec<PendingUpload>,
    next_texture_generation: u64,
    /// Stream Tube patch: textures whose last tile was removed, kept for reuse.
    /// Video frames are images larger than an atlas page, so each one gets a
    /// texture of its own; without this every frame created a full-size texture
    /// and released one. Oldest first, at most `MAX_SPARE_TEXTURES`.
    spare_textures: Vec<WgpuAtlasTexture>,
}

/// Stream Tube patch: how many freed textures `spare_textures` keeps. Two covers
/// a video whose frame is replaced every paint, with one frame still on screen.
const MAX_SPARE_TEXTURES: usize = 2;

pub struct WgpuTextureInfo {
    pub view: wgpu::TextureView,
    /// Distinguishes new textures that reuse an [`AtlasTextureId`].
    pub generation: u64,
}

impl WgpuAtlas {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        color_texture_format: wgpu::TextureFormat,
    ) -> Self {
        let max_texture_size = device.limits().max_texture_dimension_2d;
        WgpuAtlas(Mutex::new(AtlasState::new(WgpuAtlasTextures {
            device,
            queue,
            max_texture_size,
            color_texture_format,
            storage: WgpuAtlasStorage::default(),
            pending_uploads: Vec::new(),
            next_texture_generation: 0,
            spare_textures: Vec::new(),
        })))
    }

    pub fn from_context(context: &WgpuContext) -> Self {
        Self::new(
            context.device.clone(),
            context.queue.clone(),
            context.color_texture_format(),
        )
    }

    pub fn before_frame(&self) {
        let mut lock = self.0.lock();
        lock.backend.flush_uploads();
    }

    /// Returns the view backing `id`, or `None` once every tile in it has been
    /// removed. A scene can still reference such a texture when a cached view
    /// replays a paint from before the image was dropped, so callers must skip
    /// those sprites rather than assume the texture exists.
    pub fn get_texture_info(&self, id: AtlasTextureId) -> Option<WgpuTextureInfo> {
        let lock = self.0.lock();
        let texture = lock.backend.storage.get(id)?;
        Some(WgpuTextureInfo {
            view: texture.view.clone(),
            generation: texture.generation,
        })
    }

    /// Clears all cached textures and tiles, forcing them to be recreated.
    /// Use this for incremental recovery when the device is still valid.
    pub fn clear(&self) {
        self.0.lock().clear(|textures| {
            textures.storage = WgpuAtlasStorage::default();
            textures.pending_uploads.clear();
            // Stream Tube patch: start over without the spare textures too.
            textures.spare_textures.clear();
        });
    }

    /// Handles device lost by clearing all textures and cached tiles.
    /// The atlas will lazily recreate textures as needed on subsequent frames.
    pub fn handle_device_lost(&self, context: &WgpuContext) {
        self.0.lock().clear(|textures| {
            textures.device = context.device.clone();
            textures.queue = context.queue.clone();
            textures.color_texture_format = context.color_texture_format();
            textures.storage = WgpuAtlasStorage::default();
            textures.pending_uploads.clear();
            // Stream Tube patch: spare textures belong to the lost device.
            textures.spare_textures.clear();
        });
    }
}

impl PlatformAtlas for WgpuAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        self.0.lock().get_or_insert_with(key, build)
    }

    fn remove(&self, key: &AtlasKey) {
        self.0.lock().remove(key);
    }
}

impl AtlasBackend for WgpuAtlasTextures {
    fn insert(
        &mut self,
        kind: AtlasTextureKind,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<AtlasTile> {
        let tile = self.allocate(size, kind).context("failed to allocate")?;
        self.upload_texture(tile.texture_id, tile.bounds, bytes);
        Ok(tile)
    }

    fn remove(&mut self, tile: AtlasTile) {
        let id = tile.texture_id;
        let Some(texture_slot) = self.storage[id.kind].textures.get_mut(id.index as usize) else {
            return;
        };

        if let Some(mut texture) = texture_slot.take() {
            texture.allocator.deallocate(tile.tile_id.into());
            texture.decrement_ref_count();
            if texture.is_unreferenced() {
                self.pending_uploads
                    .retain(|upload| upload.id != texture.id);
                self.storage[id.kind]
                    .free_list
                    .push(texture.id.index as usize);
                // Stream Tube patch: keep the texture for the next one of its size
                // and kind instead of releasing it. Its pending uploads are gone.
                if self.spare_textures.len() >= MAX_SPARE_TEXTURES {
                    self.spare_textures.remove(0);
                }
                self.spare_textures.push(texture);
            } else {
                *texture_slot = Some(texture);
            }
        }
    }
}

impl WgpuAtlasTextures {
    fn allocate(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Option<AtlasTile> {
        {
            let textures = &mut self.storage[texture_kind];

            if let Some(tile) = textures
                .iter_mut()
                .rev()
                .find_map(|texture| texture.allocate(size))
            {
                return Some(tile);
            }
        }

        let texture = self.push_texture(size, texture_kind);
        texture.allocate(size)
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> &mut WgpuAtlasTexture {
        const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(1024),
            height: DevicePixels(1024),
        };
        let max_texture_size = self.max_texture_size as i32;
        let max_atlas_size = Size {
            width: DevicePixels(max_texture_size),
            height: DevicePixels(max_texture_size),
        };

        let size = min_size.min(&max_atlas_size).max(&DEFAULT_ATLAS_SIZE);
        let format = match kind {
            AtlasTextureKind::Monochrome => wgpu::TextureFormat::R8Unorm,
            AtlasTextureKind::Subpixel | AtlasTextureKind::Polychrome => self.color_texture_format,
        };

        // Stream Tube patch: reuse a spare texture of exactly this size, kind and
        // format. Its allocator is empty (every tile was removed); clearing it
        // resets it. It gets a new generation below, like a new texture, so the
        // renderer rebuilds its bind group.
        let etagere_size = device_size_to_etagere(size);
        let spare = self
            .spare_textures
            .iter()
            .position(|t| {
                t.id.kind == kind && t.format == format && t.allocator.size() == etagere_size
            })
            .map(|ix| self.spare_textures.remove(ix));
        let (texture, view, allocator) = if let Some(spare) = spare {
            let mut allocator = spare.allocator;
            allocator.clear();
            (spare.texture, spare.view, allocator)
        } else {
            let (texture, view) = self.create_texture(size, format);
            (texture, view, BucketedAtlasAllocator::new(etagere_size))
        };

        let texture_list = &mut self.storage[kind];
        let index = texture_list.free_list.pop();
        let generation = self.next_texture_generation;
        self.next_texture_generation = self.next_texture_generation.wrapping_add(1);

        let atlas_texture = WgpuAtlasTexture {
            id: AtlasTextureId {
                index: index.unwrap_or(texture_list.textures.len()) as u32,
                kind,
            },
            generation,
            allocator,
            format,
            texture,
            view,
            live_atlas_keys: 0,
        };

        if let Some(ix) = index {
            texture_list.textures[ix] = Some(atlas_texture);
            texture_list
                .textures
                .get_mut(ix)
                .and_then(|t| t.as_mut())
                .expect("texture must exist")
        } else {
            texture_list.textures.push(Some(atlas_texture));
            texture_list
                .textures
                .last_mut()
                .and_then(|t| t.as_mut())
                .expect("texture must exist")
        }
    }

    /// Stream Tube patch: texture and view creation, moved out of `push_texture`
    /// unchanged so a spare texture can be used instead.
    fn create_texture(
        &self,
        size: Size<DevicePixels>,
        format: wgpu::TextureFormat,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: size.width.0 as u32,
                height: size.height.0 as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    fn upload_texture(&mut self, id: AtlasTextureId, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        // Stream Tube patch: when the bytes are already in the texture's format,
        // hand them to the queue now instead of copying them into a pending
        // upload (`bytes` only lives for this call). `write_texture` stages its
        // own copy and applies it before the next submit, like `flush_uploads`
        // does. Whether a texture swizzles depends only on its format, so its
        // uploads all take the same path and stay in order.
        if let Some(texture) = self.storage.get(id) {
            if !needs_swizzle(texture.format) {
                write_tile(&self.queue, texture, bounds, bytes);
                return;
            }
        }

        let data = self
            .storage
            .get(id)
            .map(|texture| swizzle_upload_data(bytes, texture.format))
            .unwrap_or_else(|| bytes.to_vec());

        self.pending_uploads
            .push(PendingUpload { id, bounds, data });
    }

    fn flush_uploads(&mut self) {
        for upload in self.pending_uploads.drain(..) {
            let Some(texture) = self.storage.get(upload.id) else {
                continue;
            };
            write_tile(&self.queue, texture, upload.bounds, &upload.data);
        }
    }
}

/// Stream Tube patch: the `write_texture` call from `flush_uploads`, unchanged,
/// so `upload_texture` can use it too.
fn write_tile(
    queue: &wgpu::Queue,
    texture: &WgpuAtlasTexture,
    bounds: Bounds<DevicePixels>,
    data: &[u8],
) {
    let bytes_per_pixel = texture.bytes_per_pixel();

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture.texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: bounds.origin.x.0 as u32,
                y: bounds.origin.y.0 as u32,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bounds.size.width.0 as u32 * bytes_per_pixel as u32),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width: bounds.size.width.0 as u32,
            height: bounds.size.height.0 as u32,
            depth_or_array_layers: 1,
        },
    );
}

#[derive(Default)]
struct WgpuAtlasStorage {
    monochrome_textures: AtlasTextureList<WgpuAtlasTexture>,
    subpixel_textures: AtlasTextureList<WgpuAtlasTexture>,
    polychrome_textures: AtlasTextureList<WgpuAtlasTexture>,
}

impl ops::Index<AtlasTextureKind> for WgpuAtlasStorage {
    type Output = AtlasTextureList<WgpuAtlasTexture>;
    fn index(&self, kind: AtlasTextureKind) -> &Self::Output {
        match kind {
            AtlasTextureKind::Monochrome => &self.monochrome_textures,
            AtlasTextureKind::Subpixel => &self.subpixel_textures,
            AtlasTextureKind::Polychrome => &self.polychrome_textures,
        }
    }
}

impl ops::IndexMut<AtlasTextureKind> for WgpuAtlasStorage {
    fn index_mut(&mut self, kind: AtlasTextureKind) -> &mut Self::Output {
        match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Subpixel => &mut self.subpixel_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
        }
    }
}

impl WgpuAtlasStorage {
    fn get(&self, id: AtlasTextureId) -> Option<&WgpuAtlasTexture> {
        self[id.kind]
            .textures
            .get(id.index as usize)
            .and_then(|t| t.as_ref())
    }
}

struct WgpuAtlasTexture {
    id: AtlasTextureId,
    generation: u64,
    allocator: BucketedAtlasAllocator,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    format: wgpu::TextureFormat,
    live_atlas_keys: u32,
}

impl WgpuAtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.allocate(device_size_to_etagere(size))?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            padding: 0,
            bounds: Bounds {
                origin: etagere_point_to_device(allocation.rectangle.min),
                size,
            },
        };
        self.live_atlas_keys += 1;
        Some(tile)
    }

    fn bytes_per_pixel(&self) -> u8 {
        match self.format {
            wgpu::TextureFormat::R8Unorm => 1,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm => 4,
            _ => 4,
        }
    }

    fn decrement_ref_count(&mut self) {
        self.live_atlas_keys -= 1;
    }

    fn is_unreferenced(&self) -> bool {
        self.live_atlas_keys == 0
    }
}

/// Stream Tube patch: whether `swizzle_upload_data` changes bytes for `format`.
fn needs_swizzle(format: wgpu::TextureFormat) -> bool {
    format == wgpu::TextureFormat::Rgba8Unorm
}

fn swizzle_upload_data(bytes: &[u8], format: wgpu::TextureFormat) -> Vec<u8> {
    match format {
        wgpu::TextureFormat::Rgba8Unorm => {
            let mut data = bytes.to_vec();
            for pixel in data.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            data
        }
        _ => bytes.to_vec(),
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;
    use gpui::block_on;
    use gpui::{ImageId, RenderImageParams};
    use std::sync::Arc;

    fn test_device_and_queue() -> anyhow::Result<(Arc<wgpu::Device>, Arc<wgpu::Queue>)> {
        block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::all(),
                flags: wgpu::InstanceFlags::default(),
                backend_options: wgpu::BackendOptions::default(),
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                display: None,
            });
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                    apply_limit_buckets: false,
                })
                .await
                .map_err(|error| anyhow::anyhow!("failed to request adapter: {error}"))?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("wgpu_atlas_test_device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits())
                        .using_alignment(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                    trace: wgpu::Trace::Off,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                })
                .await
                .map_err(|error| anyhow::anyhow!("failed to request device: {error}"))?;
            Ok((Arc::new(device), Arc::new(queue)))
        })
    }

    #[test]
    fn before_frame_skips_uploads_for_removed_texture() -> anyhow::Result<()> {
        let (device, queue) = test_device_and_queue()?;

        let atlas = WgpuAtlas::new(device, queue, wgpu::TextureFormat::Bgra8Unorm);
        let key = AtlasKey::Image(RenderImageParams {
            image_id: ImageId(1),
            frame_index: 0,
        });
        let size = Size {
            width: DevicePixels(1),
            height: DevicePixels(1),
        };
        let mut build = || Ok(Some((size, Cow::Owned(vec![0, 0, 0, 255]))));

        // Regression test: before the fix, this panicked in flush_uploads
        atlas
            .get_or_insert_with(key.clone(), &mut build)?
            .expect("tile should be created");
        atlas.remove(&key);
        atlas.before_frame();
        Ok(())
    }

    #[test]
    fn remove_deallocates_tile_space_for_reuse() -> anyhow::Result<()> {
        let (device, queue) = test_device_and_queue()?;
        let atlas = WgpuAtlas::new(device, queue, wgpu::TextureFormat::Bgra8Unorm);

        let small = Size {
            width: DevicePixels(64),
            height: DevicePixels(64),
        };
        let big = Size {
            width: DevicePixels(700),
            height: DevicePixels(700),
        };

        let make_key = |image_id: usize| {
            AtlasKey::Image(RenderImageParams {
                image_id: ImageId(image_id),
                frame_index: 0,
            })
        };
        let insert = |key: AtlasKey, size: Size<DevicePixels>| {
            let byte_count = (size.width.0 as usize) * (size.height.0 as usize) * 4;
            atlas
                .get_or_insert_with(key, &mut || {
                    Ok(Some((size, Cow::Owned(vec![0u8; byte_count]))))
                })
                .expect("allocation should succeed")
                .expect("callback returns Some")
        };

        let keeper_key = make_key(1);
        let big_key_a = make_key(2);
        let big_key_b = make_key(3);

        let keeper_tile = insert(keeper_key, small);
        let tile_a = insert(big_key_a.clone(), big);
        assert_eq!(keeper_tile.texture_id, tile_a.texture_id);

        atlas.remove(&big_key_a);
        let tile_b = insert(big_key_b, big);
        assert_eq!(tile_b.texture_id, keeper_tile.texture_id);
        Ok(())
    }

    #[test]
    fn reused_texture_id_has_new_generation() -> anyhow::Result<()> {
        let (device, queue) = test_device_and_queue()?;
        let atlas = WgpuAtlas::new(device, queue, wgpu::TextureFormat::Bgra8Unorm);
        let size = Size {
            width: DevicePixels(700),
            height: DevicePixels(700),
        };
        let make_key = |image_id| {
            AtlasKey::Image(RenderImageParams {
                image_id: ImageId(image_id),
                frame_index: 0,
            })
        };
        let insert = |key: AtlasKey| {
            atlas
                .get_or_insert_with(key, &mut || {
                    Ok(Some((
                        size,
                        Cow::Owned(vec![0; size.width.0 as usize * size.height.0 as usize * 4]),
                    )))
                })
                .expect("allocation should succeed")
                .expect("callback returns Some")
        };

        let first_key = make_key(1);
        let first_tile = insert(first_key.clone());
        let first_generation = atlas
            .get_texture_info(first_tile.texture_id)
            .context("first texture should exist")?
            .generation;
        atlas.remove(&first_key);
        assert!(atlas.get_texture_info(first_tile.texture_id).is_none());

        let second_tile = insert(make_key(2));
        let second_generation = atlas
            .get_texture_info(second_tile.texture_id)
            .context("second texture should exist")?
            .generation;

        assert_eq!(second_tile.texture_id, first_tile.texture_id);
        assert_ne!(second_generation, first_generation);
        Ok(())
    }

    // Stream Tube patch: uploads that need no swizzle go to the queue at once;
    // the others still wait for `before_frame`.
    #[test]
    fn only_swizzled_uploads_are_deferred() -> anyhow::Result<()> {
        let (device, queue) = test_device_and_queue()?;
        let size = Size {
            width: DevicePixels(2),
            height: DevicePixels(2),
        };
        let key = AtlasKey::Image(RenderImageParams {
            image_id: ImageId(1),
            frame_index: 0,
        });
        for (format, pending) in [
            (wgpu::TextureFormat::Bgra8Unorm, 0),
            (wgpu::TextureFormat::Rgba8Unorm, 1),
        ] {
            let atlas = WgpuAtlas::new(device.clone(), queue.clone(), format);
            atlas
                .get_or_insert_with(key.clone(), &mut || {
                    Ok(Some((size, Cow::Owned(vec![0x80; 16]))))
                })?
                .expect("tile should be created");
            assert_eq!(atlas.0.lock().backend.pending_uploads.len(), pending);
            atlas.before_frame();
            assert!(atlas.0.lock().backend.pending_uploads.is_empty());
        }
        Ok(())
    }

    // Stream Tube patch: a freed texture is reused, with a new generation, for the
    // next image of the same size, and not for one of another size.
    #[test]
    fn freed_texture_is_reused_for_same_size() -> anyhow::Result<()> {
        let (device, queue) = test_device_and_queue()?;
        let atlas = WgpuAtlas::new(device, queue, wgpu::TextureFormat::Bgra8Unorm);
        let frame = Size {
            width: DevicePixels(1920),
            height: DevicePixels(1080),
        };
        let other = Size {
            width: DevicePixels(1280),
            height: DevicePixels(1100),
        };
        let make_key = |image_id| {
            AtlasKey::Image(RenderImageParams {
                image_id: ImageId(image_id),
                frame_index: 0,
            })
        };
        let insert = |image_id, size: Size<DevicePixels>| {
            atlas
                .get_or_insert_with(make_key(image_id), &mut || {
                    Ok(Some((
                        size,
                        Cow::Owned(vec![0; size.width.0 as usize * size.height.0 as usize * 4]),
                    )))
                })
                .expect("allocation should succeed")
                .expect("callback returns Some")
        };
        let texture_of = |tile: &AtlasTile| {
            let lock = atlas.0.lock();
            let texture = lock
                .backend
                .storage
                .get(tile.texture_id)
                .expect("tile's texture exists");
            (texture.texture.clone(), texture.generation)
        };

        let tile_a = insert(1, frame);
        let (texture_a, generation_a) = texture_of(&tile_a);
        atlas.remove(&make_key(1));
        assert_eq!(atlas.0.lock().backend.spare_textures.len(), 1);

        let tile_b = insert(2, other);
        assert_ne!(texture_of(&tile_b).0, texture_a);

        let tile_c = insert(3, frame);
        let (texture_c, generation_c) = texture_of(&tile_c);
        assert_eq!(texture_c, texture_a);
        assert_ne!(generation_c, generation_a);
        assert_eq!(tile_c.bounds.origin, Point::default());
        assert!(atlas.0.lock().backend.spare_textures.is_empty());

        for image_id in 4..10 {
            insert(image_id, frame);
            atlas.remove(&make_key(image_id));
        }
        assert!(atlas.0.lock().backend.spare_textures.len() <= MAX_SPARE_TEXTURES);
        atlas.before_frame();
        Ok(())
    }

    #[test]
    fn swizzle_upload_data_preserves_bgra_uploads() {
        let input = vec![0x10, 0x20, 0x30, 0x40];
        assert_eq!(
            swizzle_upload_data(&input, wgpu::TextureFormat::Bgra8Unorm),
            input
        );
    }

    #[test]
    fn swizzle_upload_data_converts_bgra_to_rgba() {
        let input = vec![0x10, 0x20, 0x30, 0x40, 0xAA, 0xBB, 0xCC, 0xDD];
        assert_eq!(
            swizzle_upload_data(&input, wgpu::TextureFormat::Rgba8Unorm),
            vec![0x30, 0x20, 0x10, 0x40, 0xCC, 0xBB, 0xAA, 0xDD]
        );
    }
}
