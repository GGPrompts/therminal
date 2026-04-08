//! GPU side of widget pre-rasterization (tn-npd).
//!
//! * `WidgetRenderer` owns a tiny textured-quad wgpu pipeline that samples
//!   pre-rasterized pixmaps uploaded as `Rgba8UnormSrgb` textures.
//! * `WidgetManager` owns the freshness cache + the `WidgetRasterizer` and
//!   is the single entry point consumers talk to: `upsert()` checks the
//!   incoming spec's `data_hash` against the cached entry's hash and only
//!   re-rasterizes / re-uploads when the hash has actually changed.
//! * `CachedWidget` is what the cache holds per `WidgetId` — the texture,
//!   bind group, the last-seen data hash, and the current on-screen rect.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use tiny_skia::Pixmap;
use wgpu::util::DeviceExt;

use super::WidgetId;
use super::rasterizer::{WidgetRasterizer, WidgetSpec};

// ── Vertex type for the textured-quad pipeline ───────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct WidgetVertex {
    position: [f32; 2],
    uv: [f32; 2],
}

const WIDGET_VERTEX_ATTRS: &[wgpu::VertexAttribute] = &[
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

fn widget_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<WidgetVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: WIDGET_VERTEX_ATTRS,
    }
}

const WIDGET_SHADER: &str = r#"
struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
};
struct VertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@group(0) @binding(0) var t_widget: texture_2d<f32>;
@group(0) @binding(1) var s_widget: sampler;

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    var out: VertexOut;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(t_widget, s_widget, in.uv);
}
"#;

// ── CachedWidget ─────────────────────────────────────────────────────────

/// A cached, rasterized-and-uploaded widget ready to composite.
///
/// Each entry carries its `last_hash` so that `WidgetManager::upsert`
/// can decide whether to bypass re-rasterization. The rect coordinates
/// (`x`, `y`, `width`, `height`) are in physical surface pixels and are
/// updated on every `upsert` call (cheap — no GPU work) so position
/// changes don't force a re-upload.
pub struct CachedWidget {
    /// Hash of the data that produced the cached pixmap. Re-rasterization
    /// only happens when `spec.data_hash` differs from this value.
    pub last_hash: u64,
    /// Top-left corner x in physical pixels.
    pub x: f32,
    /// Top-left corner y in physical pixels.
    pub y: f32,
    /// Pixel width of the cached pixmap.
    pub width: u32,
    /// Pixel height of the cached pixmap.
    pub height: u32,
    /// Underlying GPU texture.
    #[allow(dead_code)]
    pub texture: wgpu::Texture,
    /// Texture view bound in the sampling bind group. Held so the
    /// bind group's weak reference stays valid — `WidgetRenderer::draw`
    /// samples through `bind_group` rather than touching this view
    /// directly, which is why it looks unused to the compiler.
    #[allow(dead_code)]
    pub view: wgpu::TextureView,
    /// Sampling bind group (texture + sampler).
    pub bind_group: wgpu::BindGroup,
}

// ── WidgetRenderer (pipeline + draw) ─────────────────────────────────────

/// Owns the wgpu pipeline state used to composite cached widget textures.
///
/// One `WidgetRenderer` is created per `GridRenderer` lifetime (cheap —
/// just a pipeline + sampler + bind group layout). `draw` can be called
/// any number of times per frame; each call samples the provided
/// `CachedWidget` as a single textured quad via `LoadOp::Load` so it
/// composites on top of whatever's already in the view.
pub struct WidgetRenderer {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
}

impl WidgetRenderer {
    /// Create a new renderer. `surface_format` must match the swapchain
    /// format of the window (typically sRGB).
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("widget_shader"),
            source: wgpu::ShaderSource::Wgsl(WIDGET_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("widget_bgl"),
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
            label: Some("widget_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("widget_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[widget_vertex_layout()],
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
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("widget_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            sampler,
            bind_group_layout,
        }
    }

    /// Upload a pixmap as a new sRGB texture + bind group.
    ///
    /// The texture is created with `Rgba8UnormSrgb` because the pixmap
    /// bytes come straight out of tiny-skia which operates in sRGB
    /// space — using the sRGB format matches the surface format and
    /// lets wgpu do the conversion automatically on sample.
    pub fn upload_pixmap(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pixmap: &Pixmap,
    ) -> (wgpu::Texture, wgpu::TextureView, wgpu::BindGroup) {
        let size = wgpu::Extent3d {
            width: pixmap.width(),
            height: pixmap.height(),
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("widget_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // tiny-skia stores pixels as premultiplied RGBA8, 4 bytes per
        // pixel. wgpu expects the same byte order; the sRGB view format
        // handles the color-space conversion.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixmap.data(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * pixmap.width()),
                rows_per_image: Some(pixmap.height()),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("widget_bind_group"),
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

        (texture, view, bind_group)
    }

    /// Composite a single cached widget as a textured quad.
    ///
    /// Uses its own command encoder + render pass with `LoadOp::Load`
    /// so it layers on top of whatever the overlay pass already drew.
    /// Callers pass physical surface dimensions so the pixel-space
    /// widget rect can be converted to NDC here.
    pub fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
        widget: &CachedWidget,
    ) {
        if widget.width == 0 || widget.height == 0 {
            return;
        }

        let sw = surface_width as f32;
        let sh = surface_height as f32;
        let x0 = 2.0 * widget.x / sw - 1.0;
        let y0 = 1.0 - 2.0 * widget.y / sh;
        let x1 = 2.0 * (widget.x + widget.width as f32) / sw - 1.0;
        let y1 = 1.0 - 2.0 * (widget.y + widget.height as f32) / sh;

        // Two triangles, CCW, UVs in 0..1 with (0,0) at top-left.
        let vertices: [WidgetVertex; 6] = [
            WidgetVertex {
                position: [x0, y0],
                uv: [0.0, 0.0],
            },
            WidgetVertex {
                position: [x1, y0],
                uv: [1.0, 0.0],
            },
            WidgetVertex {
                position: [x0, y1],
                uv: [0.0, 1.0],
            },
            WidgetVertex {
                position: [x1, y0],
                uv: [1.0, 0.0],
            },
            WidgetVertex {
                position: [x1, y1],
                uv: [1.0, 1.0],
            },
            WidgetVertex {
                position: [x0, y1],
                uv: [0.0, 1.0],
            },
        ];

        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("widget_vbuf"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("widget_draw_encoder"),
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("widget_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
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
            pass.set_bind_group(0, &widget.bind_group, &[]);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..6, 0..1);
        }

        queue.submit(std::iter::once(encoder.finish()));
    }
}

// ── WidgetManager (freshness cache) ──────────────────────────────────────

/// Freshness cache for rasterized widgets.
///
/// Consumers call `upsert(id, spec, x, y, ...)` once per frame. The
/// manager compares `spec.data_hash` against the cached entry's
/// `last_hash`:
///
/// * **Hit + same hash** → only the rect is refreshed (cheap, no GPU work).
/// * **Hit + different hash** → pixmap re-rasterized, texture re-uploaded.
/// * **Miss** → pixmap rasterized, texture uploaded, entry inserted.
///
/// See the unit tests at the bottom of this module for the exact
/// re-rasterization contract.
pub struct WidgetManager {
    rasterizer: WidgetRasterizer,
    cache: HashMap<WidgetId, CachedWidget>,
    /// Total re-rasterizations performed. Test hook for the freshness
    /// assertions — the app code doesn't need to read this. See the
    /// `freshness_*` unit tests below.
    rasterization_count: u64,
}

impl WidgetManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        Self {
            rasterizer: WidgetRasterizer::new(),
            cache: HashMap::new(),
            rasterization_count: 0,
        }
    }

    /// Insert or update a widget.
    ///
    /// Returns a reference to the cached entry if the widget is live
    /// (either freshly rasterized or hit the cache with the same hash).
    /// Returns `None` if the rasterizer rejected the spec (e.g. zero
    /// size) — callers should treat that as "skip this frame".
    #[allow(clippy::too_many_arguments)]
    pub fn upsert(
        &mut self,
        renderer: &WidgetRenderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: WidgetId,
        spec: &WidgetSpec,
        x: f32,
        y: f32,
    ) -> Option<&CachedWidget> {
        // ── Hit + same hash: update only the on-screen rect ──────────
        if let Some(existing) = self.cache.get_mut(&id)
            && existing.last_hash == spec.data_hash
        {
            existing.x = x;
            existing.y = y;
            return self.cache.get(&id);
        }

        // ── Otherwise: rasterize + upload fresh ──────────────────────
        let pixmap = self.rasterizer.rasterize_to_pixmap(spec)?;
        let (texture, view, bind_group) = renderer.upload_pixmap(device, queue, &pixmap);
        self.rasterization_count += 1;
        self.cache.insert(
            id,
            CachedWidget {
                last_hash: spec.data_hash,
                x,
                y,
                width: pixmap.width(),
                height: pixmap.height(),
                texture,
                view,
                bind_group,
            },
        );
        self.cache.get(&id)
    }

    /// Look up a cached widget by id.
    ///
    /// `#[allow(dead_code)]` because the render path today calls
    /// `upsert` once per frame and consumes its return value directly;
    /// tool-call-card / context-gauge consumers that split "update"
    /// and "draw" into separate passes will need this accessor.
    #[allow(dead_code)]
    pub fn get(&self, id: WidgetId) -> Option<&CachedWidget> {
        self.cache.get(&id)
    }

    /// Remove a widget from the cache (e.g. when its source goes away).
    #[allow(dead_code)]
    pub fn remove(&mut self, id: WidgetId) {
        self.cache.remove(&id);
    }

    /// Number of cached widgets.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Test hook: how many re-rasterizations have occurred overall.
    #[cfg(test)]
    pub(crate) fn rasterization_count(&self) -> u64 {
        self.rasterization_count
    }
}

impl Default for WidgetManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::rasterizer::{PillSpec, WidgetKind};

    /// A freshness-only cache that bypasses the wgpu upload path.
    ///
    /// The `WidgetManager::upsert` pipeline is:
    ///     cache miss / hash mismatch  →  rasterize  →  upload
    ///     cache hit + hash match      →  noop (rect refresh only)
    ///
    /// The rasterize-vs-noop decision is pure — it doesn't depend on
    /// wgpu. This shim mirrors that decision in a CPU-only helper so
    /// the freshness invariant can be unit-tested without a GPU device
    /// (which is impractical in CI — see the CLAUDE.md manual
    /// verification notes at the bottom of this file).
    struct TestCache {
        rasterizer: WidgetRasterizer,
        last_hashes: HashMap<WidgetId, u64>,
        rasterization_count: u64,
    }

    impl TestCache {
        fn new() -> Self {
            Self {
                rasterizer: WidgetRasterizer::new(),
                last_hashes: HashMap::new(),
                rasterization_count: 0,
            }
        }

        /// Mirror of `WidgetManager::upsert`'s freshness branch:
        /// re-rasterize when and only when the hash changed or the
        /// entry was missing.
        fn upsert(&mut self, id: WidgetId, spec: &WidgetSpec) {
            if let Some(&last) = self.last_hashes.get(&id)
                && last == spec.data_hash
            {
                // Same hash → skip rasterization.
                return;
            }
            // Hash changed or missing → rasterize.
            let _ = self.rasterizer.rasterize_to_pixmap(spec);
            self.last_hashes.insert(id, spec.data_hash);
            self.rasterization_count += 1;
        }

        fn rasterization_count(&self) -> u64 {
            self.rasterization_count
        }
    }

    fn spec_with(hash: u64) -> WidgetSpec {
        WidgetSpec {
            data_hash: hash,
            kind: WidgetKind::Pill(PillSpec {
                width: 120,
                height: 24,
                corner_radius: 12.0,
                background: [0.1, 0.1, 0.2, 0.9],
                border: None,
                dot: Some([0.4, 0.9, 0.6, 1.0]),
            }),
        }
    }

    #[test]
    fn freshness_rerasterizes_on_first_call() {
        let mut cache = TestCache::new();
        cache.upsert(1, &spec_with(100));
        assert_eq!(cache.rasterization_count(), 1);
    }

    #[test]
    fn freshness_same_hash_skips_rasterization() {
        let mut cache = TestCache::new();
        cache.upsert(1, &spec_with(100));
        cache.upsert(1, &spec_with(100));
        cache.upsert(1, &spec_with(100));
        assert_eq!(
            cache.rasterization_count(),
            1,
            "three upserts with the same hash must produce exactly one rasterization"
        );
    }

    #[test]
    fn freshness_different_hash_triggers_rasterization() {
        let mut cache = TestCache::new();
        cache.upsert(1, &spec_with(100));
        cache.upsert(1, &spec_with(200));
        assert_eq!(cache.rasterization_count(), 2);
    }

    #[test]
    fn freshness_interleaved_same_then_diff() {
        let mut cache = TestCache::new();
        cache.upsert(1, &spec_with(100)); // +1
        cache.upsert(1, &spec_with(100)); // noop
        cache.upsert(1, &spec_with(101)); // +1
        cache.upsert(1, &spec_with(101)); // noop
        cache.upsert(1, &spec_with(102)); // +1
        assert_eq!(cache.rasterization_count(), 3);
    }

    #[test]
    fn freshness_different_ids_tracked_independently() {
        let mut cache = TestCache::new();
        cache.upsert(1, &spec_with(100));
        cache.upsert(2, &spec_with(100));
        // Different ids → different cache entries, even with the same hash.
        assert_eq!(cache.rasterization_count(), 2);
        cache.upsert(1, &spec_with(100));
        cache.upsert(2, &spec_with(100));
        // Both hit the cache — no new rasterizations.
        assert_eq!(cache.rasterization_count(), 2);
    }

    #[test]
    fn freshness_hash_regression_rerasterizes() {
        // Walking the hash 100 → 200 → 100 should re-rasterize each
        // transition — the cache doesn't know about "previously seen"
        // hashes, it only tracks the last one.
        let mut cache = TestCache::new();
        cache.upsert(1, &spec_with(100)); // +1
        cache.upsert(1, &spec_with(200)); // +1
        cache.upsert(1, &spec_with(100)); // +1  (because last was 200)
        assert_eq!(cache.rasterization_count(), 3);
    }

    #[test]
    fn freshness_manager_starts_empty() {
        let mgr = WidgetManager::new();
        assert_eq!(mgr.len(), 0);
        assert_eq!(mgr.rasterization_count(), 0);
    }
}

// ── Manual verification (tn-npd) ─────────────────────────────────────────
//
// The WidgetRenderer + `WidgetManager::upsert` wgpu path can't be fully
// unit-tested without a live GPU device — creating a `wgpu::Device` in
// a headless test environment is fragile (Mesa llvmpipe on CI vs Vulkan
// on the developer's workstation vs Metal on CI macOS, each with their
// own quirks). The freshness logic — which is the part most likely to
// regress — is covered by pure-CPU tests above against the `TestCache`
// shim. The end-to-end path is manually verified by:
//
//   1. Launching `therminal` with an agent present in the focused pane
//      (e.g. `claude --debug` in one pane).
//   2. Observing the top-right of the window for an agent status badge:
//      a thin rounded pill with a colored status dot + a "claude · <state>"
//      label drawn via the existing overlay text renderer.
//   3. Setting `RUST_LOG=therminal_app::widgets=debug` and watching for
//      "widget_rasterized" tracing events — one per distinct agent
//      state, not one per frame.
