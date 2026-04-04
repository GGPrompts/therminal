//! Terminal cell grid renderer using glyphon.
//!
//! Reads `alacritty_terminal::Term`'s grid and renders each cell character
//! via glyphon, mapping ANSI colors to ThermalPalette where they match and
//! passing truecolor through directly. Renders the cursor as a distinct
//! visual element (inverted block).
#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::RenderableCursor;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape};

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use therminal_core::palette::Color as PaletteColor;
use therminal_core::text::glyphon_color_mode_for_surface;

use crate::color_mapping::*;
use tracing::debug;

// ── Font configuration ────────────────────────────────────────────────────

/// Font configuration for the grid renderer.
///
/// Controls the font family, size, line height, and fallback families used
/// for terminal cell rendering.
#[derive(Clone, Debug)]
pub struct FontConfig {
    /// Font family name (e.g. "JetBrains Mono", "Fira Code").
    pub family: String,
    /// Fallback font families for glyphs not found in the primary font.
    pub fallback_families: Vec<String>,
    /// Current font size in points.
    pub font_size: f32,
    /// Line height in points (derived from font_size * ratio).
    pub line_height: f32,
    /// The original font size at startup, used for reset.
    default_font_size: f32,
}

const LINE_HEIGHT_RATIO: f32 = 1.4;
const DEFAULT_FONT_SIZE: f32 = 14.0;

impl Default for FontConfig {
    fn default() -> Self {
        let font_size = DEFAULT_FONT_SIZE;
        Self {
            family: "monospace".to_string(),
            fallback_families: Vec::new(),
            font_size,
            line_height: font_size * LINE_HEIGHT_RATIO,
            default_font_size: font_size,
        }
    }
}

impl FontConfig {
    /// Create a new FontConfig with the given family and size.
    pub fn new(family: impl Into<String>, font_size: f32) -> Self {
        Self {
            family: family.into(),
            fallback_families: Vec::new(),
            font_size,
            line_height: font_size * LINE_HEIGHT_RATIO,
            default_font_size: font_size,
            ..Default::default()
        }
    }

    /// Reset font size to the startup default.
    pub fn reset_size(&mut self) {
        self.font_size = self.default_font_size;
        self.line_height = self.font_size * LINE_HEIGHT_RATIO;
    }
}

// ── Constants ─────────────────────────────────────────────────────────────

/// Terminal background — matches kitty's `background #0a0010` (palette BG).
/// Spec(0,0,0) and palette BG are suppressed in ansi_to_glyphon_bg
/// so they don't draw redundant bg rects over this clear color.
pub(crate) const TERM_BG: [f32; 4] = [10.0 / 255.0, 0.0, 16.0 / 255.0, 1.0]; // #0a0010

/// Background opacity (0.0 = fully transparent, 1.0 = fully opaque).
const BG_OPACITY: f32 = 0.92;

/// Return TERM_BG with reduced alpha for compositor transparency.
/// `pre_multiplied`: true for PreMultiplied/Inherit, false for PostMultiplied/Auto.
pub fn clear_color_for_mode(pre_multiplied: bool) -> [f32; 4] {
    if pre_multiplied {
        [
            TERM_BG[0] * BG_OPACITY,
            TERM_BG[1] * BG_OPACITY,
            TERM_BG[2] * BG_OPACITY,
            BG_OPACITY,
        ]
    } else {
        [TERM_BG[0], TERM_BG[1], TERM_BG[2], BG_OPACITY]
    }
}

// ── RenderCell — snapshot of a single grid cell ────────────────────────────

/// A lightweight snapshot of a terminal cell, suitable for lock-free rendering.
/// Created while holding the term lock, consumed by the renderer after release.
#[derive(Clone)]
pub struct RenderCell {
    /// Viewport row index (0-based).
    pub row: usize,
    /// Column index (0-based).
    pub col: usize,
    /// The character to display.
    pub c: char,
    /// Full text for the cell, including attached zero-width codepoints.
    pub text: String,
    /// Foreground color.
    pub fg: AnsiColor,
    /// Background color.
    pub bg: AnsiColor,
    /// Cell flags (BOLD, INVERSE, WIDE_CHAR, etc.).
    pub flags: Flags,
    /// Hyperlink URI from OSC 8 or regex URL detection.
    pub hyperlink: Option<String>,
}

/// Build the full rendered text for a terminal cell.
pub(crate) fn cell_display_text(c: char, zerowidth: Option<&[char]>) -> String {
    let mut text = String::with_capacity(1 + zerowidth.map_or(0, |extra| extra.len()));
    text.push(if c == '\0' { ' ' } else { c });
    for ch in zerowidth.into_iter().flatten() {
        text.push(*ch);
    }
    text
}

// ── CachedRow — cached per-row cell data ─────────────────────────────────

/// Cached cell data for a single row, used to avoid rebuilding undamaged rows.
struct CachedRow {
    cells: Vec<RenderCell>,
}

// ── Rect rendering (for cursor and cell backgrounds) ───────────────────────

const RECT_SHADER: &str = r#"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color: vec4<f32>,
};
struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ColorVertex {
    pub(crate) position: [f32; 2],
    pub(crate) color: [f32; 4],
}

static RECT_VERTEX_ATTRS: &[wgpu::VertexAttribute] = &[
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x2,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 8,
        shader_location: 1,
    },
];

fn rect_vertex_layout() -> wgpu::VertexBufferLayout<'static> {
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<ColorVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: RECT_VERTEX_ATTRS,
    }
}

// ── GridRenderer ───────────────────────────────────────────────────────────

/// GPU-accelerated terminal grid renderer.
///
/// Renders the alacritty_terminal grid via glyphon text + colored rect pipeline
/// for backgrounds and cursor.

/// How many frames between atlas trim operations (~16s at 60fps).
const ATLAS_TRIM_INTERVAL: u64 = 1000;

pub struct GridRenderer {
    // Font configuration (family, size, line_height)
    pub font_config: FontConfig,

    // Glyphon state
    pub(crate) font_system: FontSystem,
    pub(crate) swash_cache: SwashCache,
    #[allow(dead_code)]
    cache: Cache,
    pub(crate) atlas: TextAtlas,
    pub(crate) viewport: Viewport,
    pub(crate) text_renderer: TextRenderer,

    // Separate text renderer for overlays (HUD, scroll indicator, command labels)
    // to avoid clobbering the cell text vertex buffer when prepare() is called
    // multiple times within the same frame.
    pub(crate) overlay_atlas: TextAtlas,
    pub(crate) overlay_text_renderer: TextRenderer,

    // Rect pipeline for backgrounds and cursor
    pub(crate) rect_pipeline: wgpu::RenderPipeline,

    // Persistent vertex buffer for cell backgrounds / cursor / selection rects.
    rect_buf: wgpu::Buffer,
    /// Maximum number of **vertices** the persistent rect buffer can hold.
    rect_buf_capacity: u64,

    // Cell metrics (computed from font at init)
    pub cell_width: f32,
    pub cell_height: f32,

    // Padding from top-left corner of the window
    pub(crate) padding_x: f32,
    pub(crate) padding_y: f32,

    // Per-row cache of cell data for damage-based rendering.
    row_cache: Vec<Option<CachedRow>>,

    // Persistent per-cell glyphon Buffers — only rebuilt for damaged rows.
    cell_buffers: Vec<Vec<Option<Buffer>>>,

    // Track last cursor position to rebuild affected cell buffers when cursor moves.
    last_cursor_pos: Option<(usize, usize)>,

    // Frame counter for throttled atlas trimming.
    pub(crate) frame_count: u64,

    // Persistent vertex buffer (CPU-side) for cell backgrounds / cursor / selection.
    rect_verts_cpu: Vec<ColorVertex>,

    // Frame timing: rolling average over the last N frames.
    frame_times_us: Vec<u64>,
    frame_time_idx: usize,
    frame_time_sum: u64,

    /// Hyperlink URL map: (row, col) -> URL string.
    /// Rebuilt each frame from cell hyperlinks (OSC 8) and regex URL detection.
    pub hyperlink_map: HashMap<(usize, usize), String>,
}

/// Estimate the maximum number of vertices needed for the rect buffer.
fn estimate_rect_buf_vertices(cols: usize, rows: usize) -> u64 {
    let max_rects = (rows * cols) * 2 + 8;
    (max_rects as u64) * 6
}

impl GridRenderer {
    /// Create a new GridRenderer.
    ///
    /// Initializes glyphon font system, text atlas, text renderer, and
    /// the colored rect pipeline for cursor/background rendering.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        font_config: FontConfig,
    ) -> Self {
        let font_size = font_config.font_size;
        let line_height = font_config.line_height;
        let font_family = font_config.family.clone();

        // ── Glyphon setup ────────────────────────────────────────────────
        let mut font_system = FontSystem::new();
        font_system.db_mut().set_monospace_family(&font_family);
        for fb in &font_config.fallback_families {
            let found = font_system
                .db()
                .faces()
                .any(|f| f.families.iter().any(|(name, _)| name == fb));
            if found {
                tracing::info!(font = %fb, "Fallback font available");
            } else {
                tracing::warn!(font = %fb, "Fallback font not found in system");
            }
        }
        let swash_cache = SwashCache::new();
        let cache = Cache::new(device);
        let color_mode = glyphon_color_mode_for_surface(surface_format);
        let mut atlas =
            TextAtlas::with_color_mode(device, queue, &cache, surface_format, color_mode);
        let mut overlay_atlas =
            TextAtlas::with_color_mode(device, queue, &cache, surface_format, color_mode);
        let viewport = {
            let mut vp = Viewport::new(device, &cache);
            vp.update(queue, Resolution { width, height });
            vp
        };
        let text_renderer =
            TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        let overlay_text_renderer = TextRenderer::new(
            &mut overlay_atlas,
            device,
            wgpu::MultisampleState::default(),
            None,
        );

        // ── Measure cell dimensions from font metrics ────────────────────
        let metrics = Metrics::new(font_size, line_height);
        let mut measure_buf = Buffer::new(&mut font_system, metrics);
        measure_buf.set_size(&mut font_system, Some(1000.0), Some(line_height * 2.0));
        measure_buf.set_text(
            &mut font_system,
            "M",
            Attrs::new().family(Family::Name(&font_family)),
            Shaping::Basic,
        );
        measure_buf.shape_until_scroll(&mut font_system, false);

        let cell_width = measure_buf
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|g| g.w)
            .unwrap_or(font_size * 0.6);

        let cell_height = line_height;

        debug!(cell_width, cell_height, "Grid cell metrics computed");

        // ── Rect pipeline ────────────────────────────────────────────────
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid_rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid_rect_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid_rect_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[rect_vertex_layout()],
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
            multiview: None,
            cache: None,
        });

        // ── Persistent rect vertex buffer ─────────────────────────────
        let padding_x = 4.0_f32;
        let padding_y = 4.0_f32;
        let usable_w = width as f32 - padding_x * 2.0;
        let usable_h = height as f32 - padding_y * 2.0;
        let cols = (usable_w / cell_width).floor().max(2.0) as usize;
        let rows = (usable_h / cell_height).floor().max(1.0) as usize;
        let rect_buf_capacity = estimate_rect_buf_vertices(cols, rows);
        let rect_buf_size = rect_buf_capacity * std::mem::size_of::<ColorVertex>() as u64;

        let rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid_rect_vbuf_persistent"),
            size: rect_buf_size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            font_config,
            font_system,
            swash_cache,
            cache,
            atlas,
            overlay_atlas,
            viewport,
            text_renderer,
            overlay_text_renderer,
            rect_pipeline,
            rect_buf,
            rect_buf_capacity,
            cell_width,
            cell_height,
            padding_x,
            padding_y,
            row_cache: Vec::new(),
            cell_buffers: Vec::new(),
            last_cursor_pos: None,
            frame_count: 0,
            rect_verts_cpu: Vec::new(),
            frame_times_us: vec![0u64; 100],
            frame_time_idx: 0,
            frame_time_sum: 0,
            hyperlink_map: HashMap::new(),
        }
    }

    /// Update the viewport resolution (call on resize).
    pub fn resize(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) {
        self.viewport.update(queue, Resolution { width, height });

        let (cols, rows) = self.grid_size(width, height);
        let new_capacity = estimate_rect_buf_vertices(cols, rows);
        if new_capacity != self.rect_buf_capacity {
            let buf_size = new_capacity * std::mem::size_of::<ColorVertex>() as u64;
            self.rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("grid_rect_vbuf_persistent"),
                size: buf_size,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.rect_buf_capacity = new_capacity;
        }

        self.atlas.trim();
        self.overlay_atlas.trim();

        self.row_cache.clear();
        self.cell_buffers.clear();
        self.last_cursor_pos = None;
    }

    /// Recalculate cell metrics after a font configuration change.
    pub fn update_font_metrics(&mut self) {
        let font_size = self.font_config.font_size;
        let line_height = self.font_config.line_height;

        let metrics = Metrics::new(font_size, line_height);
        let mut measure_buf = Buffer::new(&mut self.font_system, metrics);
        measure_buf.set_size(&mut self.font_system, Some(1000.0), Some(line_height * 2.0));
        measure_buf.set_text(
            &mut self.font_system,
            "M",
            Attrs::new().family(Family::Name(&self.font_config.family)),
            Shaping::Basic,
        );
        measure_buf.shape_until_scroll(&mut self.font_system, false);

        self.cell_width = measure_buf
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first())
            .map(|g| g.w)
            .unwrap_or(font_size * 0.6);
        self.cell_height = line_height;

        debug!(
            cell_width = self.cell_width,
            cell_height = self.cell_height,
            font_size,
            line_height,
            "Font metrics recalculated"
        );

        self.atlas.trim();
        self.overlay_atlas.trim();
        self.row_cache.clear();
        self.cell_buffers.clear();
        self.last_cursor_pos = None;
    }

    /// Calculate terminal grid dimensions (cols, rows) for a given pixel size.
    pub fn grid_size(&self, width: u32, height: u32) -> (usize, usize) {
        let usable_w = width as f32 - self.padding_x * 2.0;
        let usable_h = height as f32 - self.padding_y * 2.0;
        let cols = (usable_w / self.cell_width).floor().max(2.0) as usize;
        let rows = (usable_h / self.cell_height).floor().max(1.0) as usize;
        (cols, rows)
    }

    /// Get the horizontal padding from the window edge.
    pub fn padding_x(&self) -> f32 {
        self.padding_x
    }

    /// Get the vertical padding from the window edge.
    pub fn padding_y(&self) -> f32 {
        self.padding_y
    }

    /// Render the terminal grid with damage tracking.
    ///
    /// Takes pre-collected `RenderCell` snapshots (only from damaged rows when
    /// partial damage is available) and cursor info.
    /// `damaged_rows`: None means full redraw; Some(set) means only those rows changed.
    pub fn render(
        &mut self,
        cells: &[RenderCell],
        cursor: &RenderableCursor,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        damaged_rows: Option<&HashSet<usize>>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        // ── Update row cache ────────────────────────────────────────────
        if self.row_cache.len() != screen_lines {
            self.row_cache.resize_with(screen_lines, || None);
        }

        let mut new_row_cells: Vec<Vec<RenderCell>> = vec![Vec::new(); screen_lines];
        for cell in cells {
            if cell.row < screen_lines {
                new_row_cells[cell.row].push(RenderCell {
                    row: cell.row,
                    col: cell.col,
                    c: cell.c,
                    text: cell.text.clone(),
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    hyperlink: cell.hyperlink.clone(),
                });
            }
        }

        for (row_idx, row_cells) in new_row_cells.into_iter().enumerate() {
            let is_damaged = match damaged_rows {
                None => true,
                Some(set) => set.contains(&row_idx),
            };
            if is_damaged {
                if row_cells.is_empty() {
                    self.row_cache[row_idx] = None;
                } else {
                    self.row_cache[row_idx] = Some(CachedRow { cells: row_cells });
                }
            }
        }

        self.render_from_cache(
            cursor,
            screen_lines,
            selection,
            display_offset,
            damaged_rows,
            device,
            queue,
            encoder,
            target_view,
            surface_width,
            surface_height,
        );
    }

    /// Render using only the existing row cache (no new cell data).
    pub fn render_cached(
        &mut self,
        cursor: &RenderableCursor,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let empty = HashSet::new();
        self.render_from_cache(
            cursor,
            screen_lines,
            selection,
            display_offset,
            Some(&empty),
            device,
            queue,
            encoder,
            target_view,
            surface_width,
            surface_height,
        );
    }

    /// Internal: render the terminal grid from the row cache.
    fn render_from_cache(
        &mut self,
        cursor: &RenderableCursor,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        damaged_rows: Option<&HashSet<usize>>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        let frame_start = Instant::now();

        let sw = surface_width as f32;
        let sh = surface_height as f32;

        // ── Collect background rects from all cached rows ───────────────
        let mut bg_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();

        for row in self.row_cache.iter().flatten() {
            for cell in &row.cells {
                let bg_color = cell_bg_color(cell);
                if let Some(bg) = bg_color {
                    let x = self.padding_x + cell.col as f32 * self.cell_width;
                    let y = self.padding_y + cell.row as f32 * self.cell_height;
                    let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                        self.cell_width * 2.0
                    } else {
                        self.cell_width
                    };
                    bg_rects.push(([x, y, w, self.cell_height], bg));
                }
            }
        }

        // ── Cursor rect ──────────────────────────────────────────────────
        if cursor.shape != CursorShape::Hidden {
            let cursor_line = cursor.point.line.0;
            if cursor_line >= 0 && (cursor_line as usize) < screen_lines {
                let cursor_row = cursor_line as usize;
                let col_idx = cursor.point.column.0;
                let cx = self.padding_x + col_idx as f32 * self.cell_width;
                let cy = self.padding_y + cursor_row as f32 * self.cell_height;
                let wh = PaletteColor::WHITE_HOT.to_f32_array();
                let cursor_color = [wh[0], wh[1], wh[2], 0.85];

                match cursor.shape {
                    CursorShape::Block => {
                        bg_rects.push(([cx, cy, self.cell_width, self.cell_height], cursor_color));
                    }
                    CursorShape::Underline => {
                        let h = 2.0;
                        bg_rects.push((
                            [cx, cy + self.cell_height - h, self.cell_width, h],
                            cursor_color,
                        ));
                    }
                    CursorShape::Beam => {
                        bg_rects.push(([cx, cy, 2.0, self.cell_height], cursor_color));
                    }
                    CursorShape::HollowBlock => {
                        let t = 1.0;
                        bg_rects.push(([cx, cy, self.cell_width, t], cursor_color));
                        bg_rects.push((
                            [cx, cy + self.cell_height - t, self.cell_width, t],
                            cursor_color,
                        ));
                        bg_rects.push(([cx, cy, t, self.cell_height], cursor_color));
                        bg_rects.push((
                            [cx + self.cell_width - t, cy, t, self.cell_height],
                            cursor_color,
                        ));
                    }
                    CursorShape::Hidden => {}
                }
            }
        }

        // ── Selection highlight rects ────────────────────────────────────
        if let Some(sel) = selection {
            let sel_color = PaletteColor::ACCENT_COOL.to_f32_array();
            let sel_highlight = [sel_color[0], sel_color[1], sel_color[2], 0.35];

            for row in self.row_cache.iter().flatten() {
                for cell in &row.cells {
                    let grid_line = Line(cell.row as i32 - display_offset as i32);
                    let point = Point::new(grid_line, Column(cell.col));
                    if sel.contains(point) {
                        let x = self.padding_x + cell.col as f32 * self.cell_width;
                        let y = self.padding_y + cell.row as f32 * self.cell_height;
                        let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                            self.cell_width * 2.0
                        } else {
                            self.cell_width
                        };
                        bg_rects.push(([x, y, w, self.cell_height], sel_highlight));
                    }
                }
            }
        }

        // ── Hyperlink underline rects + map rebuild ────────────────────
        self.hyperlink_map.clear();
        {
            let link_color = PaletteColor::ACCENT_COOL.to_f32_array();
            let underline_h = 1.0_f32;
            for row in self.row_cache.iter().flatten() {
                for cell in &row.cells {
                    if let Some(ref url) = cell.hyperlink {
                        self.hyperlink_map.insert((cell.row, cell.col), url.clone());
                        let x = self.padding_x + cell.col as f32 * self.cell_width;
                        let y =
                            self.padding_y + cell.row as f32 * self.cell_height + self.cell_height
                                - underline_h;
                        let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                            self.cell_width * 2.0
                        } else {
                            self.cell_width
                        };
                        bg_rects.push(([x, y, w, underline_h], link_color));
                    }
                }
            }
        }

        // ── Write rect vertices into persistent buffer ──────────────────
        self.rect_verts_cpu.clear();
        for (xywh, color) in &bg_rects {
            let verts = pixel_rect_to_ndc(xywh[0], xywh[1], xywh[2], xywh[3], sw, sh, *color);
            self.rect_verts_cpu.extend_from_slice(&verts);
        }

        let rect_vertex_count = self.rect_verts_cpu.len() as u32;

        if !self.rect_verts_cpu.is_empty() {
            let needed = self.rect_verts_cpu.len() as u64;

            if needed > self.rect_buf_capacity {
                let new_capacity = needed * 2;
                let buf_size = new_capacity * std::mem::size_of::<ColorVertex>() as u64;
                self.rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("grid_rect_vbuf_persistent"),
                    size: buf_size,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.rect_buf_capacity = new_capacity;
            }

            let data = bytemuck::cast_slice::<ColorVertex, u8>(&self.rect_verts_cpu);
            queue.write_buffer(&self.rect_buf, 0, data);
        }

        // ── Rebuild only damaged per-cell glyphon Buffers ────────────────
        let metrics = Metrics::new(self.font_config.font_size, self.font_config.line_height);

        let cursor_line = cursor.point.line.0;
        let cursor_row = if cursor_line >= 0 {
            cursor_line as usize
        } else {
            usize::MAX
        };
        let cursor_col = cursor.point.column.0;

        while self.cell_buffers.len() < screen_lines {
            self.cell_buffers.push(Vec::new());
        }
        self.cell_buffers.truncate(screen_lines);

        let prev_cursor = self.last_cursor_pos;
        let full_rebuild = damaged_rows.is_none();

        for (row_idx, cached) in self.row_cache.iter().enumerate() {
            if row_idx >= screen_lines {
                break;
            }

            let needs_rebuild = if full_rebuild {
                true
            } else {
                let in_damage_set = damaged_rows
                    .map(|set| set.contains(&row_idx))
                    .unwrap_or(false);
                let is_cursor_row = row_idx == cursor_row;
                let was_cursor_row = prev_cursor.map(|(r, _)| r == row_idx).unwrap_or(false);
                in_damage_set || is_cursor_row || was_cursor_row
            };

            if !needs_rebuild {
                continue;
            }

            let row = match cached {
                Some(r) => r,
                None => {
                    self.cell_buffers[row_idx].clear();
                    continue;
                }
            };

            let row_cells = &row.cells;
            if row_cells.is_empty() {
                self.cell_buffers[row_idx].clear();
                continue;
            }

            let max_col = row_cells.iter().map(|c| c.col).max().unwrap_or(0) + 1;
            while self.cell_buffers[row_idx].len() < max_col {
                self.cell_buffers[row_idx].push(None);
            }

            for slot in self.cell_buffers[row_idx].iter_mut() {
                *slot = None;
            }

            for cell in row_cells {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let is_block_cursor = cursor.shape == CursorShape::Block
                    && cursor_col == cell.col
                    && cursor_row == cell.row;
                let fg = if is_block_cursor {
                    TERM_BG
                } else if cell.hyperlink.is_some() {
                    PaletteColor::ACCENT_COOL.to_f32_array()
                } else if cell.flags.contains(Flags::INVERSE) {
                    ansi_to_glyphon_bg(&cell.bg).unwrap_or(TERM_BG)
                } else {
                    ansi_to_glyphon_fg(&cell.fg)
                };

                if cell.c == ' ' && !is_block_cursor {
                    continue;
                }

                let buf_width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    self.cell_width * 2.0
                } else {
                    self.cell_width
                };

                let buf = self.cell_buffers[row_idx][cell.col]
                    .get_or_insert_with(|| Buffer::new(&mut self.font_system, metrics));
                buf.set_metrics(&mut self.font_system, metrics);
                buf.set_size(
                    &mut self.font_system,
                    Some(buf_width + 4.0),
                    Some(self.cell_height + 4.0),
                );

                let attrs = Attrs::new()
                    .family(Family::Name(&self.font_config.family))
                    .color(f32_to_glyph_color(fg));
                let shaping = if cell.text.is_ascii() {
                    Shaping::Basic
                } else {
                    Shaping::Advanced
                };
                buf.set_text(&mut self.font_system, &cell.text, attrs, shaping);
                buf.shape_until_scroll(&mut self.font_system, false);
            }
        }

        self.last_cursor_pos = Some((cursor_row, cursor_col));

        // ── Update viewport ──────────────────────────────────────────────
        self.viewport.update(
            queue,
            Resolution {
                width: surface_width,
                height: surface_height,
            },
        );

        // ── Prepare glyphon text from persistent cell_buffers ────────────
        let pad_x = self.padding_x;
        let pad_y = self.padding_y;
        let cw = self.cell_width;
        let ch = self.cell_height;
        let text_areas: Vec<TextArea<'_>> = self
            .cell_buffers
            .iter()
            .enumerate()
            .flat_map(|(row_idx, row)| {
                row.iter()
                    .enumerate()
                    .filter_map(move |(col_idx, opt_buf)| {
                        let buf = opt_buf.as_ref()?;
                        Some(TextArea {
                            buffer: buf,
                            left: pad_x + col_idx as f32 * cw,
                            top: pad_y + row_idx as f32 * ch,
                            scale: 1.0,
                            bounds: TextBounds {
                                left: 0,
                                top: 0,
                                right: surface_width as i32,
                                bottom: surface_height as i32,
                            },
                            default_color: GlyphColor::rgba(
                                PaletteColor::TEXT.r,
                                PaletteColor::TEXT.g,
                                PaletteColor::TEXT.b,
                                255,
                            ),
                            custom_glyphs: &[],
                        })
                    })
            })
            .collect();

        let has_text = !text_areas.is_empty();
        if has_text {
            if let Err(e) = self.text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            ) {
                tracing::warn!("glyphon prepare failed: {}", e);
            }
        }

        // ── Render pass: backgrounds + cursor rects ──────────────────────
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid_rect_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if rect_vertex_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.rect_buf.slice(..));
                pass.draw(0..rect_vertex_count, 0..1);
            }
        }

        // ── Render pass: text on top ─────────────────────────────────────
        if has_text {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("grid_text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if let Err(e) = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
            {
                tracing::warn!("glyphon render failed: {}", e);
            }
        }

        // Trim atlas periodically to free unused glyphs.
        self.frame_count += 1;
        if self.frame_count % ATLAS_TRIM_INTERVAL == 0 {
            self.atlas.trim();
            self.overlay_atlas.trim();
        }

        // ── Frame timing ────────────────────────────────────────────────
        let elapsed_us = frame_start.elapsed().as_micros() as u64;

        let idx = self.frame_time_idx % self.frame_times_us.len();
        self.frame_time_sum = self
            .frame_time_sum
            .wrapping_sub(self.frame_times_us[idx])
            .wrapping_add(elapsed_us);
        self.frame_times_us[idx] = elapsed_us;
        self.frame_time_idx = self.frame_time_idx.wrapping_add(1);

        if elapsed_us > 2000 {
            debug!(
                elapsed_us,
                frame = self.frame_count,
                "grid render frame exceeded 2ms"
            );
        }

        if self.frame_count % 100 == 0 {
            let n = self.frame_times_us.len() as u64;
            let avg_us = self.frame_time_sum / n;
            debug!(
                avg_us,
                frame = self.frame_count,
                "grid render 100-frame avg"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cell_display_text;

    #[test]
    fn cell_display_text_preserves_zero_width_codepoints() {
        assert_eq!(cell_display_text('\u{2699}', Some(&['\u{fe0f}'])), "\u{2699}\u{fe0f}");
        assert_eq!(cell_display_text('a', Some(&['\u{0301}'])), "a\u{0301}");
    }

    #[test]
    fn cell_display_text_normalizes_nul_to_space() {
        assert_eq!(cell_display_text('\0', None), " ");
    }
}
