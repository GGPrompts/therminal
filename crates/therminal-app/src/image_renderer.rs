//! Kitty graphics image renderer (tn-wdn1).
//!
//! Owns a single textured-quad wgpu pipeline that samples per-image
//! RGBA8 textures uploaded lazily on first use. One texture per
//! [`DecodedImage`] (no atlas) — images are expected to be few and
//! large, so the atlas-packing complexity isn't worth it for v1.
//!
//! ## Pipeline order
//!
//! The grid render pipeline now emits passes in this sequence:
//!
//! 1. cell backgrounds (rect pass)
//! 2. **under-text image placements** (`z < 0`)
//! 3. text glyphs (glyphon)
//! 4. **over-text image placements** (`z >= 0`)
//! 5. cursor + selection + hotspot/hyperlink underlines (rect pass)
//!
//! Splitting "cell bg" and "cursor/overlays" into two rect passes
//! lets the under/over image passes slot cleanly between them. See
//! `GridRenderer::flush_grid_render_passes` for the split.
//!
//! ## Eviction
//!
//! GPU textures live in this renderer's `HashMap<u64, ImageTexture>`
//! keyed by an opaque per-renderer id. Each [`CachedTextureSlot`]
//! holds a `Weak<DecodedImage>` pointing back at the owning
//! [`therminal_terminal::graphics::DecodedImage`] — when the image
//! store evicts it (LRU), the weak reference fails to upgrade on the
//! next frame and the slot is dropped. No explicit pending-evictions
//! channel is required — the weak-reference dance is the simplest
//! option that prevents a leak.
//!
//! ## Buffering
//!
//! A single vertex buffer is reused across frames with write-orphan
//! semantics (`queue.write_buffer`). The buffer grows on demand when
//! a frame would push more placements than the current capacity.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use bytemuck::{Pod, Zeroable};
use therminal_terminal::graphics::{DecodedImage, ImageStore, Placement, PlacementSet, TextureId};

// ── Vertex type ───────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct ImageVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

const IMAGE_VERTEX_ATTRS: &[wgpu::VertexAttribute] = &[
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 8,
        shader_location: 1,
    },
];

fn image_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ImageVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: IMAGE_VERTEX_ATTRS,
    }
}

const IMAGE_SHADER: &str = r#"
struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};
struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@group(0) @binding(0) var t_image: texture_2d<f32>;
@group(0) @binding(1) var s_image: sampler;

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    var out: VertexOut;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(t_image, s_image, in.uv);
}
"#;

/// Monotonic counter used to mint opaque `TextureId` values for the
/// image store's lazy-texture slot. The store exposes `gpu_texture:
/// OnceLock<TextureId>` with `TextureId(pub u64)`; we fill it with a
/// unique number and use that number as our internal cache key.
static NEXT_TEXTURE_ID: AtomicU64 = AtomicU64::new(1);

fn mint_texture_id() -> TextureId {
    TextureId(NEXT_TEXTURE_ID.fetch_add(1, Ordering::Relaxed))
}

/// A cached GPU texture plus a weak reference back to the owning
/// [`DecodedImage`]. When the image store evicts the image, the
/// `Weak::upgrade` fails on the next frame and the slot is dropped.
struct CachedTextureSlot {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    #[allow(dead_code)]
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// Weak reference to the image this texture was uploaded for. If
    /// the store drops the `Arc<DecodedImage>` the upgrade fails and
    /// [`ImageRenderer::evict_stale`] removes this entry.
    owner: Weak<DecodedImage>,
}

/// Compute the pixel-space rectangle and z-ordering key for a single
/// placement. Pure math helper — extracted so it can be unit-tested
/// without a GPU device.
///
/// Returns `(pixel_x, pixel_y, pixel_w, pixel_h)` in surface space.
/// `viewport_row_for_anchor` is the placement's anchor row translated
/// to the visible viewport (the caller accounts for scrollback); if
/// the anchor has scrolled out of view (negative or past the viewport
/// height) the caller should skip the placement instead of calling
/// this helper.
pub fn placement_pixel_rect(
    placement: &Placement,
    viewport_row: i64,
    pane_origin_x: f32,
    pane_origin_y: f32,
    cell_width: f32,
    cell_height: f32,
) -> (f32, f32, f32, f32) {
    let row_f = viewport_row as f32;
    let col_f = placement.anchor_col as f32;
    let px = pane_origin_x + col_f * cell_width + placement.px_x_offset as f32;
    let py = pane_origin_y + row_f * cell_height + placement.px_y_offset as f32;
    let pw = placement.cell_cols as f32 * cell_width;
    let ph = placement.cell_rows as f32 * cell_height;
    (px, py, pw, ph)
}

/// Convert a pixel-space rect to six vertex positions in normalized
/// device coordinates with UVs filling 0..1 (origin top-left).
fn rect_to_ndc_vertices(
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    surface_w: f32,
    surface_h: f32,
) -> [ImageVertex; 6] {
    let x0 = 2.0 * px / surface_w - 1.0;
    let y0 = 1.0 - 2.0 * py / surface_h;
    let x1 = 2.0 * (px + pw) / surface_w - 1.0;
    let y1 = 1.0 - 2.0 * (py + ph) / surface_h;

    [
        ImageVertex {
            position: [x0, y0],
            uv: [0.0, 0.0],
        },
        ImageVertex {
            position: [x1, y0],
            uv: [1.0, 0.0],
        },
        ImageVertex {
            position: [x0, y1],
            uv: [0.0, 1.0],
        },
        ImageVertex {
            position: [x1, y0],
            uv: [1.0, 0.0],
        },
        ImageVertex {
            position: [x1, y1],
            uv: [1.0, 1.0],
        },
        ImageVertex {
            position: [x0, y1],
            uv: [0.0, 1.0],
        },
    ]
}

/// Wgpu pipeline + per-texture cache for Kitty graphics.
pub struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
    /// Cache keyed by the opaque `TextureId.0`. Each slot carries the
    /// GPU texture plus a weak reference to the owning `DecodedImage`
    /// — slots whose `owner` fails to upgrade are dropped at the top
    /// of every frame.
    cache: HashMap<u64, CachedTextureSlot>,
    /// Persistent vertex buffer (reused across frames, grown as needed).
    vbuf: wgpu::Buffer,
    vbuf_capacity: u64,
    /// CPU-side scratch vertex buffer, reused each frame to avoid
    /// allocation churn.
    scratch: Vec<ImageVertex>,
}

impl ImageRenderer {
    /// Create a new renderer. `surface_format` must match the swapchain
    /// format (typically sRGB).
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image_shader"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[image_vertex_layout()],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Ccw,
                polygon_mode: wgpu::PolygonMode::Fill,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    // Straight-alpha blending: Kitty delivers raw RGBA,
                    // not premultiplied. Matches how glyphon's text atlas
                    // composes and how the rect pipeline's existing
                    // ALPHA_BLENDING mode treats rect colors.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        // Seed with room for ~16 placements (96 vertices). Grows on demand.
        let initial_vertex_count: u64 = 96;
        let vbuf_size = initial_vertex_count * std::mem::size_of::<ImageVertex>() as u64;
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image_vbuf_persistent"),
            size: vbuf_size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            sampler,
            bind_group_layout,
            cache: HashMap::new(),
            vbuf,
            vbuf_capacity: initial_vertex_count,
            scratch: Vec::new(),
        }
    }

    /// Drop cached texture slots whose owning `DecodedImage` has been
    /// evicted from the image store (LRU). Runs at the top of every
    /// frame — the weak-reference upgrade check is cheap.
    fn evict_stale(&mut self) {
        self.cache.retain(|_, slot| slot.owner.strong_count() > 0);
    }

    /// Upload an RGBA8 pixel buffer as a new sRGB texture + bind group
    /// and remember it under a freshly minted [`TextureId`]. Returns
    /// the new id so the caller can stash it in the image's
    /// `gpu_texture` `OnceLock`.
    fn upload_new(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &Arc<DecodedImage>,
    ) -> TextureId {
        let id = mint_texture_id();

        let size = wgpu::Extent3d {
            width: image.width.max(1),
            height: image.height.max(1),
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kitty_image_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // TODO(beads): separate pipelines for linear-raw (`f=24`/`f=32`)
            // vs sRGB-PNG (`f=100`) textures. Raw pixels are linear but are
            // currently uploaded as sRGB, which applies gamma twice.
            // Fixing this requires threading `GraphicsFormat` to the
            // uploader and minting a matching pipeline variant.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * image.width.max(1)),
                rows_per_image: Some(image.height.max(1)),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kitty_image_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.cache.insert(
            id.0,
            CachedTextureSlot {
                texture,
                view,
                bind_group,
                width: image.width,
                height: image.height,
                owner: Arc::downgrade(image),
            },
        );

        id
    }

    /// Ensure a GPU texture exists for `image`. First draw uploads;
    /// subsequent frames reuse the cached handle via the [`TextureId`]
    /// the image carries. Returns the texture id that's live in the
    /// cache, or `None` if upload was skipped (zero-size image).
    fn ensure_uploaded(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &Arc<DecodedImage>,
    ) -> Option<TextureId> {
        if image.width == 0 || image.height == 0 {
            return None;
        }

        // Fast path: image already has a TextureId stored, and our
        // cache still has the slot (not evicted).
        if let Some(existing) = image.gpu_texture.get()
            && self.cache.contains_key(&existing.0)
        {
            return Some(*existing);
        }

        // Upload a fresh texture and try to claim the image's OnceLock
        // slot. If another thread raced us and filled the OnceLock, use
        // the winner's id and drop our just-uploaded slot on the next
        // frame's eviction pass.
        let new_id = self.upload_new(device, queue, image);
        match image.gpu_texture.set(new_id) {
            Ok(()) => Some(new_id),
            Err(_) => {
                // Another uploader won. Drop our slot immediately.
                self.cache.remove(&new_id.0);
                image.gpu_texture.get().copied()
            }
        }
    }

    /// Draw every placement in `set` whose bucket matches `bucket`.
    ///
    /// Runs inside a single render pass that loads the existing
    /// swapchain contents. One draw call per placement (the quad
    /// vertices for every placement share a single persistent vertex
    /// buffer, but each placement's texture lives in its own bind
    /// group so we issue a `set_bind_group` + `draw` per placement).
    ///
    /// The `pane_origin_x` / `pane_origin_y` are the pixel origin of
    /// the pane's content area (same `padding_x`/`padding_y` used by
    /// `GridRenderer` for cell positioning). `display_offset` is the
    /// number of scrollback rows above the visible viewport, matched
    /// against the placement's `anchor_row` to decide visibility.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_bucket(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
        pane_origin_x: f32,
        pane_origin_y: f32,
        cell_width: f32,
        cell_height: f32,
        screen_lines: usize,
        display_offset: usize,
        store: &mut ImageStore,
        set: &PlacementSet,
        bucket: DrawBucket,
    ) {
        self.evict_stale();

        // Collect visible placements in draw order, filtering by bucket.
        let visible: Vec<(&Placement, f32, f32, f32, f32, TextureId)> = set
            .iter_draw_order()
            .filter(|p| match bucket {
                DrawBucket::UnderText => p.is_under_text(),
                DrawBucket::OverText => !p.is_under_text(),
            })
            .filter_map(|p| {
                // Translate scrollback row → viewport row. If the
                // placement is outside the visible strip, skip it.
                let viewport_row =
                    placement_viewport_row(p.anchor_row, display_offset, screen_lines)?;
                // The pixel data is keyed by the **transmit command**'s
                // `(image_id, placement_id)` pair — which for most
                // transmits is `(N, 0)` because clients rarely set `p=`
                // on `a=t/T`. Placements created via `a=p i=N p=M` reuse
                // the same pixels; looking up with the placement's
                // `placement_id` would miss. See tn-6005 review.
                // TODO: if a future transmit binds pixels per placement,
                // add a second lookup here that falls back to the
                // placement-specific key.
                let image = store.get(therminal_terminal::graphics::ImageId::new(
                    Some(p.image_id),
                    None,
                ))?;
                let texture_id = self.ensure_uploaded(device, queue, &image)?;
                let (px, py, pw, ph) = placement_pixel_rect(
                    p,
                    viewport_row,
                    pane_origin_x,
                    pane_origin_y,
                    cell_width,
                    cell_height,
                );
                Some((p, px, py, pw, ph, texture_id))
            })
            .collect();

        if visible.is_empty() {
            return;
        }

        // Build / grow the persistent vertex buffer.
        self.scratch.clear();
        self.scratch.reserve(visible.len() * 6);
        let sw = surface_width as f32;
        let sh = surface_height as f32;
        for (_, px, py, pw, ph, _) in &visible {
            let verts = rect_to_ndc_vertices(*px, *py, *pw, *ph, sw, sh);
            self.scratch.extend_from_slice(&verts);
        }

        let needed_vertices = self.scratch.len() as u64;
        if needed_vertices > self.vbuf_capacity {
            let new_capacity = needed_vertices * 2;
            let size_bytes = new_capacity * std::mem::size_of::<ImageVertex>() as u64;
            self.vbuf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("image_vbuf_persistent"),
                size: size_bytes,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vbuf_capacity = new_capacity;
        }
        let data = bytemuck::cast_slice::<ImageVertex, u8>(&self.scratch);
        queue.write_buffer(&self.vbuf, 0, data);

        // Emit one render pass with one draw per placement.
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(match bucket {
                DrawBucket::UnderText => "image_under_text_pass",
                DrawBucket::OverText => "image_over_text_pass",
            }),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        for (idx, (_, _, _, _, _, texture_id)) in visible.iter().enumerate() {
            let Some(slot) = self.cache.get(&texture_id.0) else {
                continue;
            };
            // Skip zero-size slots defensively (shouldn't happen given
            // the `ensure_uploaded` filter but protects the draw call).
            if slot.width == 0 || slot.height == 0 {
                continue;
            }
            pass.set_bind_group(0, &slot.bind_group, &[]);
            let base_vertex = (idx as u32) * 6;
            pass.draw(base_vertex..base_vertex + 6, 0..1);
        }
    }
}

/// Which z-bucket to draw in a single `draw_bucket` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawBucket {
    /// `z < 0` — composited between cell background and glyphs.
    UnderText,
    /// `z >= 0` — composited after glyphs and before cursor/overlays.
    OverText,
}

/// Compute the viewport-relative row for a placement's scrollback anchor.
///
/// Returns `Some(row)` when the placement's anchor falls inside the
/// visible viewport (`0..screen_lines`), or `None` when it has scrolled
/// above/below and should be skipped.
///
/// The current viewport/scrollback model in therminal keys placements
/// on the same row indices the grid cells use (0..screen_lines), so
/// for now we pass the anchor through as-is when `display_offset` is 0
/// and mark it out-of-view when the offset pushes it off-screen. This
/// matches the simple model `PlacementSet::scroll_by` assumes — any
/// richer scrollback semantics will be plumbed in a follow-up once the
/// PTY reader translates scroll events into `scroll_by` calls.
pub fn placement_viewport_row(
    anchor_row: usize,
    display_offset: usize,
    screen_lines: usize,
) -> Option<i64> {
    // Convert to signed domain so the off-top case produces a
    // negative row we can filter cleanly.
    let row = anchor_row as i64 - display_offset as i64;
    if row < 0 || row >= screen_lines as i64 {
        return None;
    }
    Some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_placement(anchor_row: usize, anchor_col: usize, cols: u32, rows: u32) -> Placement {
        Placement {
            image_id: 1,
            placement_id: 0,
            anchor_row,
            anchor_col,
            cell_rows: rows,
            cell_cols: cols,
            px_x_offset: 0,
            px_y_offset: 0,
            z_index: 0,
            created_at: 0,
        }
    }

    #[test]
    fn placement_pixel_rect_basic() {
        let p = make_placement(3, 5, 4, 2);
        let (x, y, w, h) = placement_pixel_rect(&p, 3, 10.0, 20.0, 8.0, 16.0);
        // col 5 at 8px cell width = x offset of 40 + pane origin 10 = 50
        assert!((x - 50.0).abs() < 0.01);
        // row 3 at 16px cell height = y offset of 48 + pane origin 20 = 68
        assert!((y - 68.0).abs() < 0.01);
        // 4 cells wide * 8px = 32
        assert!((w - 32.0).abs() < 0.01);
        // 2 cells tall * 16px = 32
        assert!((h - 32.0).abs() < 0.01);
    }

    #[test]
    fn placement_pixel_rect_respects_sub_cell_offsets() {
        let mut p = make_placement(0, 0, 1, 1);
        p.px_x_offset = 3;
        p.px_y_offset = 7;
        let (x, y, _, _) = placement_pixel_rect(&p, 0, 0.0, 0.0, 10.0, 20.0);
        assert!((x - 3.0).abs() < 0.01);
        assert!((y - 7.0).abs() < 0.01);
    }

    #[test]
    fn rect_to_ndc_vertices_corners() {
        let verts = rect_to_ndc_vertices(0.0, 0.0, 800.0, 600.0, 800.0, 600.0);
        // Top-left NDC
        assert!((verts[0].position[0] - -1.0).abs() < 0.01);
        assert!((verts[0].position[1] - 1.0).abs() < 0.01);
        // Bottom-right NDC
        assert!((verts[4].position[0] - 1.0).abs() < 0.01);
        assert!((verts[4].position[1] - -1.0).abs() < 0.01);
    }

    #[test]
    fn placement_viewport_row_inside_viewport() {
        assert_eq!(placement_viewport_row(5, 0, 24), Some(5));
        assert_eq!(placement_viewport_row(0, 0, 24), Some(0));
        assert_eq!(placement_viewport_row(23, 0, 24), Some(23));
    }

    #[test]
    fn placement_viewport_row_outside_viewport() {
        assert_eq!(placement_viewport_row(24, 0, 24), None);
        assert_eq!(placement_viewport_row(100, 0, 24), None);
    }

    #[test]
    fn placement_viewport_row_with_scroll() {
        // Anchor row 10, display offset 5 → visible at row 5.
        assert_eq!(placement_viewport_row(10, 5, 24), Some(5));
        // Anchor row 3, display offset 5 → scrolled above viewport.
        assert_eq!(placement_viewport_row(3, 5, 24), None);
    }

    /// Regression guard for tn-6005 review finding: the pixel data is
    /// keyed by the transmit command's `(image_id, placement_id)` pair,
    /// which is typically `(N, 0)` because `a=t` / `a=T` rarely sets
    /// `p=`. A placement created via `a=p i=N p=M` gets a non-zero
    /// `placement_id`, but the pixels still live under `(N, 0)` in the
    /// store. The renderer's lookup must therefore use `placement_id =
    /// None` (→ 0), not the placement's own `placement_id`, or images
    /// silently fail to render.
    #[test]
    fn store_lookup_for_placement_uses_transmit_key() {
        use std::sync::OnceLock;
        use therminal_terminal::graphics::{
            DecodedImage, ImageId, ImageStore, PlacementSet, RawGraphicsCommand,
        };

        let mut store = ImageStore::default();
        // Insert the pixel data under `(image_id = 7, placement_id = 0)`
        // — the normal transmit-side key.
        store.insert(
            ImageId::new(Some(7), None),
            DecodedImage {
                width: 1,
                height: 1,
                pixels: vec![0xff, 0x00, 0x00, 0xff],
                gpu_texture: OnceLock::new(),
            },
        );

        let mut set = PlacementSet::new();
        let cmd = RawGraphicsCommand::empty();
        // Create a placement via `a=p i=7 p=5` at the cursor.
        set.insert_display_at_cursor(Some(7), Some(5), Some(1), Some(1), Some(0), &cmd, 0, 0);

        // Simulate the renderer's lookup: image_id from placement,
        // placement_id = None.
        let placement = set.iter_draw_order().next().expect("placement present");
        assert_eq!(placement.image_id, 7);
        assert_eq!(placement.placement_id, 5);

        let img = store.get(ImageId::new(Some(placement.image_id), None));
        assert!(
            img.is_some(),
            "lookup via (image_id, None) must resolve the transmit-side pixels"
        );

        // Counter-check: the erroneous lookup via the placement's own
        // `placement_id` misses the transmit key.
        let missing = store.get(ImageId::new(
            Some(placement.image_id),
            Some(placement.placement_id),
        ));
        assert!(
            missing.is_none(),
            "lookup via (image_id, placement_id) must NOT hit the transmit-side key"
        );
    }

    /// Verify that creating a placement set, applying a display event,
    /// and iterating in draw order yields exactly the z-bucket the
    /// renderer expects. Uses the protocol-level types, not GPU —
    /// guards the filter predicate `draw_bucket` relies on.
    #[test]
    fn draw_bucket_split_matches_iter_draw_order() {
        let mut set = PlacementSet::new();

        // Poke them in via insert_display_at_cursor to exercise the
        // standard insertion path.
        use therminal_terminal::graphics::RawGraphicsCommand;
        let cmd = RawGraphicsCommand::empty();
        set.insert_display_at_cursor(Some(2), None, Some(1), Some(1), Some(-1), &cmd, 0, 0);
        set.insert_display_at_cursor(Some(3), None, Some(1), Some(1), Some(5), &cmd, 0, 0);

        let under_ids: Vec<u32> = set
            .iter_draw_order()
            .filter(|p| p.is_under_text())
            .map(|p| p.image_id)
            .collect();
        let over_ids: Vec<u32> = set
            .iter_draw_order()
            .filter(|p| !p.is_under_text())
            .map(|p| p.image_id)
            .collect();
        assert_eq!(under_ids, vec![2]);
        assert_eq!(over_ids, vec![3]);
    }
}
