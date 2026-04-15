//! Terminal cell grid renderer using glyphon.
//!
//! Reads `alacritty_terminal::Term`'s grid and renders each cell character
//! via glyphon, mapping ANSI colors to ThermalPalette where they match and
//! passing truecolor through directly. Renders the cursor as a distinct
//! visual element (inverted block).
#![allow(clippy::too_many_arguments)]

use therminal_core::font::PLATFORM_MONOSPACE;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::RenderableCursor;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};

use glyphon::{
    Attrs, Buffer, Cache, Color as GlyphColor, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use therminal_core::text::glyphon_color_mode_for_surface;

use crate::color_mapping::*;
use crate::pane::PaneId;
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
    /// UI chrome font family (tabs, status bar, pane headers, CSD).
    /// When empty, falls back to the grid `family`.
    pub ui_font_family: String,
}

const LINE_HEIGHT_RATIO: f32 = 1.375;
const DEFAULT_FONT_SIZE: f32 = 17.0;
const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

impl Default for FontConfig {
    fn default() -> Self {
        let font_size = DEFAULT_FONT_SIZE;
        Self {
            family: DEFAULT_FONT_FAMILY.to_string(),
            fallback_families: vec!["Noto Color Emoji".to_string()],
            font_size,
            line_height: font_size * LINE_HEIGHT_RATIO,
            default_font_size: font_size,
            ui_font_family: String::new(),
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
            ui_font_family: String::new(),
        }
    }

    /// Reset font size to the startup default.
    pub fn reset_size(&mut self) {
        self.font_size = self.default_font_size;
        self.line_height = self.font_size * LINE_HEIGHT_RATIO;
    }

    /// The effective primary family, resolving empty string to the platform
    /// monospace default.
    pub fn effective_family(&self) -> &str {
        if self.family.is_empty() {
            PLATFORM_MONOSPACE
        } else {
            &self.family
        }
    }

    /// The effective UI chrome family (tabs, status bar, pane headers, CSD).
    /// Falls back to the grid font family when `ui_font_family` is empty.
    pub fn chrome_font_family(&self) -> &str {
        if self.ui_font_family.is_empty() {
            self.effective_family()
        } else {
            &self.ui_font_family
        }
    }
}

// ── Constants ─────────────────────────────────────────────────────────────

/// Terminal background — VOID_0 from the Codex 2031 palette (#060a12).
/// Spec(0,0,0) and palette BG are suppressed in ansi_to_glyphon_bg
/// so they don't draw redundant bg rects over this clear color.
pub(crate) const TERM_BG: [f32; 4] = [6.0 / 255.0, 10.0 / 255.0, 18.0 / 255.0, 1.0]; // #060a12

/// Background opacity (0.0 = fully transparent, 1.0 = fully opaque).
#[allow(dead_code)]
const BG_OPACITY: f32 = 0.92;

/// Return TERM_BG with reduced alpha for compositor transparency.
/// `pre_multiplied`: true for PreMultiplied/Inherit, false for PostMultiplied/Auto.
#[allow(dead_code)]
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

/// Where a hyperlink came from — affects visual rendering style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HyperlinkSource {
    /// Explicit OSC 8 hyperlink from the terminal application (solid underline).
    Osc8,
    /// Regex-detected URL in terminal text (dashed underline).
    Regex,
}

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
    pub hyperlink: Option<Arc<str>>,
    /// Source of the hyperlink (OSC 8 vs regex), controls underline style.
    pub hyperlink_source: Option<HyperlinkSource>,
    /// Hotspot annotation for this cell: (kind, full matched text, is_dir).
    ///
    /// `is_dir` is `true` only when the hotspot is a `FilePath` whose target
    /// stat'd as a directory at detection time (set by
    /// `promote_directory_hotspots`). The click handler uses it to route
    /// directory hotspots through `folder_pane_command` instead of the
    /// editor fallback chain (tn-zqwg).
    pub hotspot: Option<HotspotInfo>,
}

/// Per-cell hotspot annotation: kind + full matched text + is_dir flag +
/// optional harness-resolved absolute path (tn-gidy).
///
/// Stored on each `RenderCell` and surfaced via the renderer's
/// `hotspot_map` so the click handler can branch on whether the target
/// is a directory. When the hotspot was emitted by a harness crate that
/// pre-resolved the path against the agent's cwd (e.g. Claude Code
/// `Update(crates/foo.rs)` joined against the agent working_dir), the
/// fourth tuple element carries the absolute path; the click handler
/// prefers it over the visible `text` to survive worktree hops.
pub type HotspotInfo = (
    therminal_terminal::hotspot_detection::HotspotKind,
    Arc<str>,
    bool,
    Option<Arc<str>>,
);

fn ansi_index_for_named(named: NamedColor) -> Option<usize> {
    Some(match named {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White | NamedColor::DimWhite => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => return None,
    })
}

fn resolve_fg_color(
    fg_override: Option<[f32; 4]>,
    ansi_override: Option<&[[f32; 4]; 16]>,
    color: &AnsiColor,
) -> [f32; 4] {
    match color {
        AnsiColor::Named(NamedColor::Foreground)
        | AnsiColor::Named(NamedColor::BrightForeground)
        | AnsiColor::Named(NamedColor::DimForeground) => {
            fg_override.unwrap_or_else(|| ansi_to_glyphon_fg(color))
        }
        AnsiColor::Named(named) => {
            if let Some(palette) = ansi_override
                && let Some(idx) = ansi_index_for_named(*named)
            {
                return palette[idx];
            }
            ansi_to_glyphon_fg(color)
        }
        AnsiColor::Indexed(idx) => {
            if let Some(palette) = ansi_override
                && (*idx as usize) < palette.len()
            {
                return palette[*idx as usize];
            }
            ansi_to_glyphon_fg(color)
        }
        AnsiColor::Spec(_) => ansi_to_glyphon_fg(color),
    }
}

fn resolve_bg_color(ansi_override: Option<&[[f32; 4]; 16]>, color: &AnsiColor) -> Option<[f32; 4]> {
    match color {
        AnsiColor::Named(NamedColor::Background) => None,
        AnsiColor::Named(NamedColor::Black) => None,
        AnsiColor::Named(named) => {
            if let Some(palette) = ansi_override
                && let Some(idx) = ansi_index_for_named(*named)
            {
                return Some(palette[idx]);
            }
            ansi_to_glyphon_bg(color)
        }
        AnsiColor::Indexed(idx) => {
            if *idx == 0 {
                return None;
            }
            if let Some(palette) = ansi_override
                && (*idx as usize) < palette.len()
            {
                return Some(palette[*idx as usize]);
            }
            ansi_to_glyphon_bg(color)
        }
        AnsiColor::Spec(_) => ansi_to_glyphon_bg(color),
    }
}

fn viewport_cache_matches(
    cached_line_count: Option<usize>,
    last_column_count: Option<usize>,
    last_display_offset: Option<usize>,
    columns: usize,
    screen_lines: usize,
    display_offset: usize,
) -> bool {
    cached_line_count == Some(screen_lines)
        && last_column_count == Some(columns)
        && last_display_offset == Some(display_offset)
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

/// Lightweight key capturing the properties that affect glyph shaping for a cell.
///
/// When a damaged row is rebuilt, each cell's shape key is compared against the
/// previously stored key.  If they match, the existing glyphon `Buffer` is kept
/// instead of being dropped and recreated — avoiding an expensive
/// `Buffer::new()` + `shape_until_scroll()` round-trip through cosmic-text.
#[derive(Clone, PartialEq)]
struct CellShapeKey {
    text: String,
    bold: bool,
    italic: bool,
    wide: bool,
    fg: [f32; 4],
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
///
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
    #[allow(dead_code)]
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

    // Padding from top-left corner of the window (base values from config).
    base_padding_x: f32,
    base_padding_y: f32,
    // Active padding used during rendering (includes viewport offset for split panes).
    pub(crate) padding_x: f32,
    pub(crate) padding_y: f32,

    // Per-pane row cache of cell data for damage-based rendering.
    pane_row_cache: HashMap<PaneId, Vec<Option<CachedRow>>>,

    // Per-pane persistent per-cell glyphon Buffers — only rebuilt for damaged rows.
    pane_cell_buffers: HashMap<PaneId, Vec<Vec<Option<Buffer>>>>,

    // Per-pane cell shape keys mirroring cell_buffers — used to skip reshaping
    // cells whose text, flags, and color haven't changed.
    pane_cell_shape_keys: HashMap<PaneId, Vec<Vec<Option<CellShapeKey>>>>,

    // Per-pane last cursor position to rebuild affected cell buffers when cursor moves.
    pane_last_cursor_pos: HashMap<PaneId, (usize, usize)>,

    // Per-pane last rendered display offset. Row caches are viewport-relative,
    // so a scrollback change invalidates the cached visible rows.
    pane_last_display_offset: HashMap<PaneId, usize>,

    // Per-pane last rendered column count. Width-only pane resizes can keep
    // the same row count, so line-count checks alone are not enough to decide
    // whether cached row/cell buffers are still valid after a split.
    pane_last_column_count: HashMap<PaneId, usize>,

    // The pane whose caches are currently active. Set via `set_current_pane()`.
    current_pane: Option<PaneId>,

    // Frame counter for throttled atlas trimming.
    pub(crate) frame_count: u64,

    // Persistent vertex buffer (CPU-side) for cell backgrounds / cursor / selection.
    rect_verts_cpu: Vec<ColorVertex>,

    // Frame timing: rolling average over the last N frames.
    frame_times_us: Vec<u64>,
    frame_time_idx: usize,
    frame_time_sum: u64,

    /// Hyperlink URL map: (pane_id, row, col) -> URL string.
    /// Rebuilt each frame from cell hyperlinks (OSC 8) and regex URL detection.
    pub hyperlink_map: HashMap<(PaneId, usize, usize), Arc<str>>,

    /// Hotspot map: (pane_id, row, col) -> (HotspotKind, matched text, is_dir).
    /// Rebuilt each frame from detected hotspots (file paths, errors, git refs, etc.).
    /// `is_dir` is `true` for `FilePath` hotspots that resolved to a directory
    /// when promoted via `promote_directory_hotspots` (tn-zqwg).
    pub hotspot_map: HashMap<(PaneId, usize, usize), HotspotInfo>,

    /// The pane currently being rendered. Set before each pane's render pass
    /// so that hotspot/hyperlink map entries are keyed to the correct pane.
    current_pane_id: Option<PaneId>,

    // ── Color overrides from config ──────────────────────────────────────
    /// Override for background clear color (from config.colors.background).
    bg_override: Option<[f32; 4]>,
    /// Override for default foreground color (from config.colors.foreground).
    fg_override: Option<[f32; 4]>,
    /// Optional ANSI palette override (16-color table from config.colors.ansi).
    ansi_override: Option<[[f32; 4]; 16]>,

    /// Theme-aware chrome palette (tn-g7oo). Holds resolved RGBA values for
    /// every chrome / overlay role (focus border, separators, header bg,
    /// status bar, tab bar, hotspot underlines, selection, cursor, ...).
    /// Built by `apply_color_overrides` from the active `ColorsConfig` —
    /// chrome modules read this directly so theme reloads re-skin the UI
    /// immediately.
    pub chrome_palette: therminal_core::palette::ChromePalette,

    /// Cache for overlay text buffers (status bar, pane headers, tab bar).
    /// Key: slot name (e.g. "header_index_0", "status_center", "tab_1").
    /// Value: (cache_key, shaped Buffer). The cache_key encodes text + width + style
    /// so we only re-shape when inputs actually change.
    pub(crate) overlay_cache: HashMap<String, (String, Buffer)>,

    /// Pattern-engine widget matches collected during the current frame's
    /// render pass (tn-068b). Cleared at the start of each frame via
    /// `clear_frame_maps()`, populated by `extend_hotspots_from_patterns`
    /// in `render.rs`, consumed by `draw_widget_overlays` in
    /// `render_driver.rs`.
    pub(crate) pattern_widget_sink: Vec<crate::widgets::pattern_widget::PatternWidgetMatch>,

    /// Scale factor for UI chrome text (status bar, pane headers, tab bar,
    /// overlays). `1.0` is default. Applied by `chrome_font_size()`.
    /// Set from `config.accessibility.ui_text_scale` (tn-avjv.6).
    pub(crate) ui_text_scale: f32,
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
        padding: f32,
    ) -> Self {
        let font_size = font_config.font_size;
        let line_height = font_config.line_height;
        let font_family = font_config.effective_family().to_string();

        // ── Glyphon setup ────────────────────────────────────────────────
        let mut font_system = FontSystem::new();
        let total_fonts = font_system.db().faces().count();
        let primary_found = font_system
            .db()
            .faces()
            .any(|f| f.families.iter().any(|(name, _)| name == &font_family));
        tracing::info!(
            total_fonts,
            primary_found,
            family = %font_family,
            "Font system initialized"
        );
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
            &Attrs::new().family(Family::Name(&font_family)),
            Shaping::Basic,
            None,
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
            immediate_size: 0,
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
            multiview_mask: None,
            cache: None,
        });

        // ── Persistent rect vertex buffer ─────────────────────────────
        let padding_x = padding;
        let padding_y = padding;
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
            base_padding_x: padding_x,
            base_padding_y: padding_y,
            padding_x,
            padding_y,
            pane_row_cache: HashMap::new(),
            pane_cell_buffers: HashMap::new(),
            pane_cell_shape_keys: HashMap::new(),
            pane_last_cursor_pos: HashMap::new(),
            pane_last_display_offset: HashMap::new(),
            pane_last_column_count: HashMap::new(),
            current_pane: None,
            frame_count: 0,
            rect_verts_cpu: Vec::new(),
            frame_times_us: vec![0u64; 100],
            frame_time_idx: 0,
            frame_time_sum: 0,
            hyperlink_map: HashMap::new(),
            hotspot_map: HashMap::new(),
            current_pane_id: None,
            bg_override: None,
            fg_override: None,
            ansi_override: None,
            chrome_palette: therminal_core::palette::ChromePalette::default(),
            overlay_cache: HashMap::new(),
            pattern_widget_sink: Vec::new(),
            ui_text_scale: 1.0,
        }
    }

    /// Update the UI text scale factor (from `config.accessibility.ui_text_scale`).
    pub fn set_ui_text_scale(&mut self, scale: f32) {
        self.ui_text_scale = scale.clamp(0.5, 3.0);
    }

    /// Return a font size scaled by `ui_text_scale` for chrome text rendering.
    ///
    /// Chrome renderers (status bar, pane headers, tab bar) should call this
    /// instead of computing font size directly so that the accessibility
    /// `ui_text_scale` setting takes effect.
    pub fn chrome_font_size(&self, base_size: f32) -> f32 {
        (base_size * self.ui_text_scale).max(8.0)
    }

    /// Set padding (both x and y) from config. Call before resize to take effect.
    pub fn set_padding(&mut self, padding: f32) {
        self.base_padding_x = padding;
        self.base_padding_y = padding;
        self.padding_x = padding;
        self.padding_y = padding;
    }

    /// Set viewport offset for split-pane rendering.
    /// Call before render(), then call `restore_padding()` after.
    pub fn set_viewport_offset(&mut self, x: f32, y: f32) {
        self.padding_x = x;
        self.padding_y = y;
    }

    /// Restore padding to base config values after split-pane render.
    pub fn restore_padding(&mut self) {
        self.padding_x = self.base_padding_x;
        self.padding_y = self.base_padding_y;
    }

    /// Apply color overrides from the config's `ColorsConfig`.
    ///
    /// Rebuilds both the bg/fg/ansi overrides used by terminal cell color
    /// resolution and the runtime [`therminal_core::palette::ChromePalette`]
    /// used by every chrome module (tn-g7oo). Called from `init()`,
    /// `apply_config()`, and the hot-reload event handler — themes can
    /// re-skin the entire window without a restart.
    pub fn apply_color_overrides(&mut self, colors: &therminal_core::config::ColorsConfig) {
        self.bg_override = colors
            .background
            .as_deref()
            .and_then(therminal_core::config::ColorsConfig::parse_hex)
            .map(|c| c.to_f32_array());
        self.fg_override = colors
            .foreground
            .as_deref()
            .and_then(therminal_core::config::ColorsConfig::parse_hex)
            .map(|c| c.to_f32_array());
        self.ansi_override = colors.ansi.as_ref().and_then(|entries| {
            if entries.len() < 16 {
                return None;
            }
            let mut out = [[0.0; 4]; 16];
            for (i, raw) in entries.iter().take(16).enumerate() {
                let parsed = therminal_core::config::ColorsConfig::parse_hex(raw.as_str())?;
                out[i] = parsed.to_f32_array();
            }
            Some(out)
        });

        // Resolve the chrome palette from defaults + per-role overrides.
        // This is the single point where chrome theming is plumbed through;
        // every chrome module reads `renderer.chrome_palette` directly so a
        // hot-reload of `[colors]` re-skins the entire UI immediately.
        self.chrome_palette = colors.chrome_palette();

        self.clear_render_caches();
    }

    fn map_fg_color(&self, color: &AnsiColor) -> [f32; 4] {
        resolve_fg_color(self.fg_override, self.ansi_override.as_ref(), color)
    }

    fn map_bg_color(&self, color: &AnsiColor) -> Option<[f32; 4]> {
        resolve_bg_color(self.ansi_override.as_ref(), color)
    }

    /// Get the resolved background clear color as `[f32; 4]`, respecting config overrides.
    pub fn resolved_bg(&self) -> [f32; 4] {
        self.bg_override.unwrap_or(TERM_BG)
    }

    /// Get the resolved cursor color as `[f32; 4]`, sourced from
    /// `chrome_palette.cursor` (theme-aware, tn-g7oo).
    pub fn resolved_cursor_color(&self) -> [f32; 4] {
        self.chrome_palette.cursor
    }

    /// Get the resolved selection highlight color as `[f32; 4]`, sourced
    /// from `chrome_palette.selection` (theme-aware, tn-g7oo).
    pub fn resolved_selection_color(&self) -> [f32; 4] {
        self.chrome_palette.selection
    }

    /// Drop all per-pane render caches so the next frame fully rebuilds.
    ///
    /// Also drains `hotspot_map` (tn-yxiz): entries are keyed by
    /// `(PaneId, row, col)` and the damage-aware update in `render_from_cache`
    /// only rewrites cells that re-render. After a resize (window or font),
    /// the column count changes and every row is re-rendered from scratch,
    /// but stale entries at pre-resize columns beyond the new right edge
    /// would otherwise remain and draw underlines past the content area.
    /// Dropping the whole map forces a clean rebuild against current grid
    /// metrics. `hyperlink_map` is rebuilt every frame in `clear_frame_maps`
    /// so it doesn't need explicit invalidation here.
    pub fn clear_render_caches(&mut self) {
        self.pane_row_cache.clear();
        self.pane_cell_buffers.clear();
        self.pane_cell_shape_keys.clear();
        self.pane_last_cursor_pos.clear();
        self.pane_last_display_offset.clear();
        self.pane_last_column_count.clear();
        self.hotspot_map.clear();
        // Chrome text buffers cache GlyphColor in shaped Attrs — stale
        // after a palette change.  Force reshape on the next frame.
        self.overlay_cache.clear();
    }

    /// Returns true when the cached viewport rows for `pane_id` match the
    /// current visible scrollback offset and line count.
    pub fn pane_cache_matches_viewport(
        &self,
        pane_id: PaneId,
        columns: usize,
        screen_lines: usize,
        display_offset: usize,
    ) -> bool {
        viewport_cache_matches(
            self.pane_row_cache.get(&pane_id).map(Vec::len),
            self.pane_last_column_count.get(&pane_id).copied(),
            self.pane_last_display_offset.get(&pane_id).copied(),
            columns,
            screen_lines,
            display_offset,
        )
    }

    /// Reconstruct row text from the cached `RenderCell`s for a pane.
    ///
    /// Used by the early-exit (undamaged) render path so the pattern engine
    /// can re-emit widget matches without paying for a fresh cell collection
    /// pass (tn-qyyp). Returns one `String` per cached row, with cells laid
    /// out by column. Returns an empty `Vec` if the pane has no cached rows.
    pub fn cached_row_texts(&self, pane_id: PaneId) -> Vec<String> {
        let Some(rows) = self.pane_row_cache.get(&pane_id) else {
            return Vec::new();
        };
        rows.iter()
            .map(|row| match row {
                Some(cached) => {
                    let mut chars: Vec<char> = Vec::new();
                    for cell in &cached.cells {
                        if cell.col >= chars.len() {
                            chars.resize(cell.col + 1, ' ');
                        }
                        chars[cell.col] = cell.c;
                    }
                    chars.into_iter().collect()
                }
                None => String::new(),
            })
            .collect()
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

        self.clear_render_caches();
    }

    /// Replace the font configuration and recalculate metrics.
    ///
    /// Used by the config hot-reload path to apply new font settings
    /// without recreating the entire renderer.  Caller should follow up
    /// with `resize()` to re-create the rect buffer for the new grid size.
    pub fn update_font(
        &mut self,
        new_config: FontConfig,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) {
        self.font_config = new_config;
        self.font_system
            .db_mut()
            .set_monospace_family(self.font_config.effective_family());
        self.update_font_metrics();
        self.resize(device, queue, width, height);
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
            &Attrs::new().family(Family::Name(self.font_config.effective_family())),
            Shaping::Basic,
            None,
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
        self.clear_render_caches();
    }

    /// Adjust the font size by `delta` points (e.g. +1.0 or -1.0).
    ///
    /// Clamps to the range 8.0..=32.0, recalculates cell metrics, and clears
    /// all caches to force a full rebuild.  Returns the new font size.
    pub fn adjust_font_size(&mut self, delta: f32) -> f32 {
        let new_size = (self.font_config.font_size + delta).clamp(8.0, 32.0);
        self.font_config.font_size = new_size;
        self.font_config.line_height = new_size * LINE_HEIGHT_RATIO;
        self.update_font_metrics();
        new_size
    }

    /// Reset the font size to the startup default and recalculate metrics.
    ///
    /// Returns the restored font size.
    pub fn reset_font_size(&mut self) -> f32 {
        self.font_config.reset_size();
        self.update_font_metrics();
        self.font_config.font_size
    }

    /// Check whether a font family name exists in the system font database.
    pub fn is_font_available(&self, family: &str) -> bool {
        self.font_system.db().faces().any(|f| {
            f.families
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(family))
        })
    }

    /// Remove the cached state for a specific pane (e.g. when a pane is closed).
    pub fn remove_pane_cache(&mut self, pane_id: PaneId) {
        self.pane_row_cache.remove(&pane_id);
        self.pane_cell_buffers.remove(&pane_id);
        self.pane_cell_shape_keys.remove(&pane_id);
        // Clean up hotspot_map entries for the removed pane (since the map
        // is no longer cleared every frame).
        self.hotspot_map.retain(|&(pid, _, _), _| pid != pane_id);
        self.pane_last_cursor_pos.remove(&pane_id);
        self.pane_last_display_offset.remove(&pane_id);
        self.pane_last_column_count.remove(&pane_id);
    }

    /// Clear per-frame maps at the start of a new frame, before any panes
    /// are rendered.
    ///
    /// The hotspot_map is NOT cleared here — it uses damage-aware incremental
    /// updates in `render_from_cache()` to avoid rebuilding entries for
    /// undamaged rows. Stale pane entries are cleaned up in `remove_pane_cache()`.
    pub fn clear_frame_maps(&mut self) {
        // hotspot_map: damage-aware incremental update (see render_from_cache)
        self.hyperlink_map.clear();
        self.pattern_widget_sink.clear();
    }

    /// Set the pane ID for the pane about to be rendered. Map entries
    /// inserted during `render()` will be keyed to this pane. Also switches
    /// the per-pane row/cell caches to this pane (creating them if needed).
    pub fn set_current_pane(&mut self, pane_id: PaneId) {
        self.current_pane_id = Some(pane_id);
        self.current_pane = Some(pane_id);
        // Ensure cache entries exist for this pane.
        self.pane_row_cache.entry(pane_id).or_default();
        self.pane_cell_buffers.entry(pane_id).or_default();
        self.pane_cell_shape_keys.entry(pane_id).or_default();
    }

    /// Calculate terminal grid dimensions (cols, rows) for a given pixel size.
    pub fn grid_size(&self, width: u32, height: u32) -> (usize, usize) {
        let usable_w = width as f32 - self.base_padding_x * 2.0;
        let usable_h = height as f32 - self.base_padding_y * 2.0;
        let cols = (usable_w / self.cell_width).floor().max(2.0) as usize;
        let rows = (usable_h / self.cell_height).floor().max(1.0) as usize;
        (cols, rows)
    }

    /// Get the base horizontal padding (config value, not viewport-adjusted).
    #[allow(clippy::misnamed_getters)]
    pub fn padding_x(&self) -> f32 {
        self.base_padding_x
    }

    /// Get the base vertical padding (config value, not viewport-adjusted).
    #[allow(clippy::misnamed_getters)]
    pub fn padding_y(&self) -> f32 {
        self.base_padding_y
    }

    /// Render the terminal grid with damage tracking.
    ///
    /// Takes pre-collected `RenderCell` snapshots (only from damaged rows when
    /// partial damage is available) and cursor info.
    /// `damaged_rows`: None means full redraw; Some(slice) means only rows marked `true` changed.
    pub fn render(
        &mut self,
        cells: &[RenderCell],
        cursor: &RenderableCursor,
        columns: usize,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        damaged_rows: Option<&[bool]>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
        surface_width: u32,
        surface_height: u32,
    ) {
        // ── Update row cache for current pane ────────────────────────────
        let pane_id = self.current_pane.unwrap_or(0);
        let row_cache = self.pane_row_cache.entry(pane_id).or_default();
        if row_cache.len() != screen_lines {
            row_cache.resize_with(screen_lines, || None);
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
                    hyperlink_source: cell.hyperlink_source,
                    hotspot: cell.hotspot.clone(),
                });
            }
        }

        for (row_idx, row_cells) in new_row_cells.into_iter().enumerate() {
            let is_damaged = match damaged_rows {
                None => true,
                Some(d) => d.get(row_idx).copied().unwrap_or(false),
            };
            if is_damaged {
                if row_cells.is_empty() {
                    row_cache[row_idx] = None;
                } else {
                    row_cache[row_idx] = Some(CachedRow { cells: row_cells });
                }
            }
        }

        self.render_from_cache(
            cursor,
            columns,
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
        columns: usize,
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
        let empty: Vec<bool> = Vec::new();
        self.render_from_cache(
            cursor,
            columns,
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
        columns: usize,
        screen_lines: usize,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        damaged_rows: Option<&[bool]>,
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

        // ── Temporarily take per-pane caches out of the HashMaps ────────
        // This avoids borrow-checker conflicts when accessing font_system
        // and other &mut self fields alongside the per-pane cache vecs.
        let pane_id = self.current_pane.unwrap_or(0);
        let row_cache = self.pane_row_cache.remove(&pane_id).unwrap_or_default();
        let mut cell_buffers = self.pane_cell_buffers.remove(&pane_id).unwrap_or_default();
        let mut cell_shape_keys = self
            .pane_cell_shape_keys
            .remove(&pane_id)
            .unwrap_or_default();
        let prev_cursor = self.pane_last_cursor_pos.get(&pane_id).copied();

        // ── 1. Collect background, cursor, selection rects ──────────────
        let mut bg_rects: Vec<([f32; 4], [f32; 4])> = Vec::new();
        self.collect_cell_bg_rects(&row_cache, &mut bg_rects);
        self.add_cursor_rects(cursor, screen_lines, &mut bg_rects);
        self.add_selection_rects(selection, display_offset, &row_cache, &mut bg_rects);

        // ── 2. Hyperlink + hotspot underlines (and map updates) ─────────
        let map_pane_id = self.current_pane_id.unwrap_or(0);
        self.process_hyperlinks(map_pane_id, &row_cache, &mut bg_rects);
        self.process_hotspots(map_pane_id, &row_cache, damaged_rows, &mut bg_rects);

        // ── 3. Upload rect vertices into the persistent GPU buffer ──────
        let rect_vertex_count = self.upload_rect_buffer(&bg_rects, sw, sh, device, queue);

        // ── 4. Rebuild damaged per-cell glyphon buffers ─────────────────
        let cursor_line = cursor.point.line.0;
        let cursor_row = if cursor_line >= 0 {
            cursor_line as usize
        } else {
            usize::MAX
        };
        let cursor_col = cursor.point.column.0;
        self.rebuild_cell_buffers(
            cursor,
            cursor_row,
            cursor_col,
            screen_lines,
            damaged_rows,
            prev_cursor,
            &row_cache,
            &mut cell_buffers,
            &mut cell_shape_keys,
        );

        // ── 5. Persist per-pane bookkeeping fields ──────────────────────
        self.pane_last_cursor_pos
            .insert(pane_id, (cursor_row, cursor_col));
        self.pane_last_display_offset
            .insert(pane_id, display_offset);
        self.pane_last_column_count.insert(pane_id, columns);

        // ── 6. Update viewport, prepare text, emit render passes ────────
        self.viewport.update(
            queue,
            Resolution {
                width: surface_width,
                height: surface_height,
            },
        );

        // Theme-aware (tn-g7oo): cell-default text color falls back to the
        // chrome `chrome_fg` role so an override of `[colors] chrome_fg`
        // re-skins any cell that doesn't carry an explicit fg attr. The
        // existing `[colors] foreground` override still applies first when
        // present (it short-circuits the chrome_fg fallback).
        let default_text_color = self.fg_override.unwrap_or_else(|| {
            let c = self.chrome_palette.chrome_fg;
            [
                c.r as f32 / 255.0,
                c.g as f32 / 255.0,
                c.b as f32 / 255.0,
                1.0,
            ]
        });
        let text_areas = build_grid_text_areas(
            &cell_buffers,
            self.padding_x,
            self.padding_y,
            self.cell_width,
            self.cell_height,
            surface_width,
            surface_height,
            default_text_color,
        );
        let has_text = !text_areas.is_empty();
        if has_text
            && let Err(e) = self.text_renderer.prepare(
                device,
                queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
        {
            tracing::warn!("glyphon prepare failed: {}", e);
        }
        self.flush_grid_render_passes(rect_vertex_count, has_text, encoder, target_view);

        // ── 7. Atlas trim + frame timing housekeeping ───────────────────
        self.frame_count += 1;
        if self.frame_count.is_multiple_of(ATLAS_TRIM_INTERVAL) {
            self.atlas.trim();
            self.overlay_atlas.trim();
        }
        self.update_frame_timing(frame_start);

        // ── 8. Restore per-pane caches back into the HashMaps ───────────
        self.pane_row_cache.insert(pane_id, row_cache);
        self.pane_cell_buffers.insert(pane_id, cell_buffers);
        self.pane_cell_shape_keys.insert(pane_id, cell_shape_keys);
    }

    /// Collect cell-background rects from every cached row. Cells with no
    /// `bg` (transparent / default) are skipped so the persistent rect
    /// buffer only carries the geometry that actually paints.
    fn collect_cell_bg_rects(
        &self,
        row_cache: &[Option<CachedRow>],
        bg_rects: &mut Vec<([f32; 4], [f32; 4])>,
    ) {
        for row in row_cache.iter().flatten() {
            for cell in &row.cells {
                if let Some(bg) = cell_bg_color(cell) {
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
    }

    /// Append the cursor rect(s) to `bg_rects`. Block / underline / beam /
    /// hollow-block all share the same anchor formula but emit different
    /// geometry. Hidden cursors emit nothing.
    fn add_cursor_rects(
        &self,
        cursor: &RenderableCursor,
        screen_lines: usize,
        bg_rects: &mut Vec<([f32; 4], [f32; 4])>,
    ) {
        if cursor.shape == CursorShape::Hidden {
            return;
        }
        let cursor_line = cursor.point.line.0;
        if cursor_line < 0 || (cursor_line as usize) >= screen_lines {
            return;
        }
        let cursor_row = cursor_line as usize;
        let col_idx = cursor.point.column.0;
        let cx = self.padding_x + col_idx as f32 * self.cell_width;
        let cy = self.padding_y + cursor_row as f32 * self.cell_height;
        let cursor_color = self.resolved_cursor_color();

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

    /// Append selection-highlight rects for any cell that lies inside the
    /// active `SelectionRange`. No-op when `selection` is `None`.
    fn add_selection_rects(
        &self,
        selection: Option<&SelectionRange>,
        display_offset: usize,
        row_cache: &[Option<CachedRow>],
        bg_rects: &mut Vec<([f32; 4], [f32; 4])>,
    ) {
        let Some(sel) = selection else { return };
        let sel_highlight = self.resolved_selection_color();

        for row in row_cache.iter().flatten() {
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

    /// Update `hyperlink_map` and emit underline rects for every cell that
    /// carries an OSC 8 or regex-detected URL. OSC 8 hyperlinks get a solid
    /// underline; regex URLs get a 3px-on / 2px-off dashed underline so the
    /// two sources are visually distinguishable.
    fn process_hyperlinks(
        &mut self,
        pane_id: PaneId,
        row_cache: &[Option<CachedRow>],
        bg_rects: &mut Vec<([f32; 4], [f32; 4])>,
    ) {
        // Theme-aware (tn-g7oo) — sourced from chrome_palette.hyperlink so a
        // light-theme override re-skins regex URL + OSC 8 underlines together.
        let link_color = self.chrome_palette.hyperlink;
        let underline_h = 1.0_f32;
        let dash_on = 3.0_f32;
        let dash_off = 2.0_f32;
        for row in row_cache.iter().flatten() {
            for cell in &row.cells {
                let Some(ref url) = cell.hyperlink else {
                    continue;
                };
                self.hyperlink_map
                    .insert((pane_id, cell.row, cell.col), Arc::clone(url));
                let x = self.padding_x + cell.col as f32 * self.cell_width;
                let y = self.padding_y + cell.row as f32 * self.cell_height + self.cell_height
                    - underline_h;
                let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                    self.cell_width * 2.0
                } else {
                    self.cell_width
                };
                let is_regex = cell.hyperlink_source == Some(HyperlinkSource::Regex);
                if is_regex {
                    let mut offset = 0.0;
                    while offset < w {
                        let seg_w = (w - offset).min(dash_on);
                        bg_rects.push(([x + offset, y, seg_w, underline_h], link_color));
                        offset += dash_on + dash_off;
                    }
                } else {
                    bg_rects.push(([x, y, w, underline_h], link_color));
                }
            }
        }
    }

    /// Damage-aware update of `hotspot_map` plus dotted-underline rect
    /// emission. Each damaged row's stale entries are evicted and
    /// re-inserted from fresh cell data, while undamaged rows keep their
    /// existing map entries — the underline pass then runs over every
    /// cached row regardless of damage so visible decorations stay stable.
    fn process_hotspots(
        &mut self,
        pane_id: PaneId,
        row_cache: &[Option<CachedRow>],
        damaged_rows: Option<&[bool]>,
        bg_rects: &mut Vec<([f32; 4], [f32; 4])>,
    ) {
        // Map update: evict + re-insert for damaged rows only.
        for (row_idx, cached_row) in row_cache.iter().enumerate() {
            let is_damaged = match damaged_rows {
                None => true,
                Some(d) => d.get(row_idx).copied().unwrap_or(false),
            };
            if !is_damaged {
                continue;
            }
            if let Some(row) = cached_row {
                for cell in &row.cells {
                    self.hotspot_map.remove(&(pane_id, cell.row, cell.col));
                }
                for cell in &row.cells {
                    if let Some((ref kind, ref full_text, is_dir, ref resolved)) = cell.hotspot
                        && cell.hyperlink.as_ref().is_none_or(|h| h.starts_with("file://"))
                    {
                        self.hotspot_map.insert(
                            (pane_id, cell.row, cell.col),
                            (
                                kind.clone(),
                                Arc::clone(full_text),
                                is_dir,
                                resolved.clone(),
                            ),
                        );
                    }
                }
            }
        }

        // Underline emission: dotted (2px on / 2px off) for every cell
        // that carries a hotspot but no non-file hyperlink (non-file://
        // hyperlinks already drew a different underline above; file://
        // URIs defer to Therminal's richer hotspot features).
        let underline_h = 1.0_f32;
        let dot_on = 2.0_f32;
        let dot_off = 2.0_f32;
        for row in row_cache.iter().flatten() {
            for cell in &row.cells {
                if let Some((ref kind, _, _, _)) = cell.hotspot
                    && cell.hyperlink.as_ref().is_none_or(|h| h.starts_with("file://"))
                {
                    let hotspot_color =
                        crate::color_mapping::hotspot_kind_color(kind, &self.chrome_palette);
                    let x = self.padding_x + cell.col as f32 * self.cell_width;
                    let y = self.padding_y + cell.row as f32 * self.cell_height + self.cell_height
                        - underline_h;
                    let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                        self.cell_width * 2.0
                    } else {
                        self.cell_width
                    };
                    let mut offset = 0.0;
                    while offset < w {
                        let seg_w = (w - offset).min(dot_on);
                        bg_rects.push(([x + offset, y, seg_w, underline_h], hotspot_color));
                        offset += dot_on + dot_off;
                    }
                }
            }
        }
    }

    /// Flatten the collected `bg_rects` into the CPU vertex buffer, grow
    /// the persistent GPU buffer if needed, and queue a `write_buffer` to
    /// upload the new data. Returns the vertex count for the subsequent
    /// draw call.
    fn upload_rect_buffer(
        &mut self,
        bg_rects: &[([f32; 4], [f32; 4])],
        sw: f32,
        sh: f32,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> u32 {
        self.rect_verts_cpu.clear();
        for (xywh, color) in bg_rects {
            let verts = pixel_rect_to_ndc(xywh[0], xywh[1], xywh[2], xywh[3], sw, sh, *color);
            self.rect_verts_cpu.extend_from_slice(&verts);
        }

        let rect_vertex_count = self.rect_verts_cpu.len() as u32;
        if self.rect_verts_cpu.is_empty() {
            return rect_vertex_count;
        }

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

        rect_vertex_count
    }

    /// Rebuild the persistent per-cell `Buffer`s for any row marked as
    /// damaged (or the whole viewport when `damaged_rows` is `None`). The
    /// shape-key cache lets us skip rows whose text/flags/color match the
    /// previous frame, so a typical typing edit only re-shapes the row the
    /// cursor is on.
    #[allow(clippy::too_many_arguments)]
    fn rebuild_cell_buffers(
        &mut self,
        cursor: &RenderableCursor,
        cursor_row: usize,
        cursor_col: usize,
        screen_lines: usize,
        damaged_rows: Option<&[bool]>,
        prev_cursor: Option<(usize, usize)>,
        row_cache: &[Option<CachedRow>],
        cell_buffers: &mut Vec<Vec<Option<Buffer>>>,
        cell_shape_keys: &mut Vec<Vec<Option<CellShapeKey>>>,
    ) {
        let metrics = Metrics::new(self.font_config.font_size, self.font_config.line_height);

        while cell_buffers.len() < screen_lines {
            cell_buffers.push(Vec::new());
        }
        cell_buffers.truncate(screen_lines);
        while cell_shape_keys.len() < screen_lines {
            cell_shape_keys.push(Vec::new());
        }
        cell_shape_keys.truncate(screen_lines);

        let full_rebuild = damaged_rows.is_none();

        for (row_idx, cached) in row_cache.iter().enumerate() {
            if row_idx >= screen_lines {
                break;
            }

            let needs_rebuild = if full_rebuild {
                true
            } else {
                let in_damage_set = damaged_rows
                    .map(|d| d.get(row_idx).copied().unwrap_or(false))
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
                    cell_buffers[row_idx].clear();
                    cell_shape_keys[row_idx].clear();
                    continue;
                }
            };

            self.rebuild_row(
                row,
                row_idx,
                cursor,
                cursor_row,
                cursor_col,
                metrics,
                cell_buffers,
                cell_shape_keys,
            );
        }
    }

    /// Rebuild a single damaged row's per-cell glyphon buffers, sharing
    /// `font_system` with the existing slot when the shape key matches.
    /// Pulled out of `rebuild_cell_buffers` so the per-row body stays
    /// readable on its own.
    #[allow(clippy::too_many_arguments)]
    fn rebuild_row(
        &mut self,
        row: &CachedRow,
        row_idx: usize,
        cursor: &RenderableCursor,
        cursor_row: usize,
        cursor_col: usize,
        metrics: Metrics,
        cell_buffers: &mut [Vec<Option<Buffer>>],
        cell_shape_keys: &mut [Vec<Option<CellShapeKey>>],
    ) {
        let row_cells = &row.cells;
        if row_cells.is_empty() {
            cell_buffers[row_idx].clear();
            cell_shape_keys[row_idx].clear();
            return;
        }

        let max_col = row_cells.iter().map(|c| c.col).max().unwrap_or(0) + 1;
        while cell_buffers[row_idx].len() < max_col {
            cell_buffers[row_idx].push(None);
        }
        while cell_shape_keys[row_idx].len() < max_col {
            cell_shape_keys[row_idx].push(None);
        }

        // Track which columns have visible content so we can clear stale buffers.
        let mut occupied_cols = vec![false; max_col];

        for cell in row_cells {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            let is_block_cursor = cursor.shape == CursorShape::Block
                && cursor_col == cell.col
                && cursor_row == cell.row;
            let fg = if is_block_cursor {
                self.resolved_bg()
            } else if cell.hyperlink.is_some() {
                // Theme-aware hyperlink text color (tn-g7oo) — matches the
                // underline color so the cell glyphs read as "linked".
                self.chrome_palette.hyperlink
            } else if cell.flags.contains(Flags::INVERSE) {
                self.map_bg_color(&cell.bg).unwrap_or(self.resolved_bg())
            } else {
                self.map_fg_color(&cell.fg)
            };

            if cell.c == ' ' && !is_block_cursor {
                if cell.col < cell_buffers[row_idx].len() {
                    cell_buffers[row_idx][cell.col] = None;
                    cell_shape_keys[row_idx][cell.col] = None;
                }
                if cell.col < occupied_cols.len() {
                    occupied_cols[cell.col] = true;
                }
                continue;
            }

            if cell.col < occupied_cols.len() {
                occupied_cols[cell.col] = true;
            }

            let new_key = CellShapeKey {
                text: cell.text.clone(),
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                wide: cell.flags.contains(Flags::WIDE_CHAR),
                fg,
            };

            if let Some(old_key) = cell_shape_keys[row_idx]
                .get(cell.col)
                .and_then(|k| k.as_ref())
                && *old_key == new_key
            {
                continue;
            }

            let buf_width = if new_key.wide {
                self.cell_width * 2.0
            } else {
                self.cell_width
            };

            let buf = cell_buffers[row_idx][cell.col]
                .get_or_insert_with(|| Buffer::new(&mut self.font_system, metrics));
            buf.set_metrics(&mut self.font_system, metrics);
            buf.set_size(
                &mut self.font_system,
                Some(buf_width + 4.0),
                Some(self.cell_height + 4.0),
            );

            let attrs = Attrs::new()
                .family(Family::Name(self.font_config.effective_family()))
                .color(f32_to_glyph_color(fg));
            let shaping = if cell.text.is_ascii() {
                Shaping::Basic
            } else {
                Shaping::Advanced
            };
            buf.set_text(&mut self.font_system, &cell.text, &attrs, shaping, None);
            buf.shape_until_scroll(&mut self.font_system, false);

            cell_shape_keys[row_idx][cell.col] = Some(new_key);
        }

        // Clear buffers for columns no longer occupied.
        for col in 0..cell_buffers[row_idx].len() {
            if col < occupied_cols.len() && !occupied_cols[col] {
                cell_buffers[row_idx][col] = None;
                cell_shape_keys[row_idx][col] = None;
            }
        }

        // Truncate trailing slots if the row shrank.
        cell_buffers[row_idx].truncate(max_col);
        cell_shape_keys[row_idx].truncate(max_col);
    }

    /// Emit the two render passes that paint the grid: a rect pass for
    /// backgrounds / cursor / underlines, then (optionally) a glyphon text
    /// pass for the cell glyphs on top. Both load the existing swapchain
    /// contents so prior pane render passes are preserved.
    fn flush_grid_render_passes(
        &self,
        rect_vertex_count: u32,
        has_text: bool,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &wgpu::TextureView,
    ) {
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
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if rect_vertex_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.rect_buf.slice(..));
                pass.draw(0..rect_vertex_count, 0..1);
            }
        }

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
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if let Err(e) = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
            {
                tracing::warn!("glyphon render failed: {}", e);
            }
        }
    }

    /// Update the rolling 100-frame timing window and emit a debug log
    /// when the just-finished frame exceeded the 2 ms target or every 100
    /// frames as a checkpoint.
    fn update_frame_timing(&mut self, frame_start: Instant) {
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

        if self.frame_count.is_multiple_of(100) {
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

/// Build the per-frame TextArea list from the persistent per-cell
/// glyphon `Buffer`s. Pure function so it can be unit-tested without
/// constructing a `GridRenderer`, and so the borrow on `cell_buffers`
/// stays scoped tightly inside `render_from_cache`.
fn build_grid_text_areas<'a>(
    cell_buffers: &'a [Vec<Option<Buffer>>],
    pad_x: f32,
    pad_y: f32,
    cell_width: f32,
    cell_height: f32,
    surface_width: u32,
    surface_height: u32,
    default_text_color: [f32; 4],
) -> Vec<TextArea<'a>> {
    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };
    let default_color = GlyphColor::rgba(
        (default_text_color[0] * 255.0) as u8,
        (default_text_color[1] * 255.0) as u8,
        (default_text_color[2] * 255.0) as u8,
        (default_text_color[3] * 255.0) as u8,
    );
    cell_buffers
        .iter()
        .enumerate()
        .flat_map(move |(row_idx, row)| {
            row.iter()
                .enumerate()
                .filter_map(move |(col_idx, opt_buf)| {
                    let buf = opt_buf.as_ref()?;
                    Some(TextArea {
                        buffer: buf,
                        left: pad_x + col_idx as f32 * cell_width,
                        top: pad_y + row_idx as f32 * cell_height,
                        scale: 1.0,
                        bounds,
                        default_color,
                        custom_glyphs: &[],
                    })
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::FontConfig;
    use super::cell_display_text;
    use super::resolve_fg_color;
    use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
    use therminal_core::font::PLATFORM_MONOSPACE;

    #[test]
    fn empty_family_resolves_to_platform_default() {
        let cfg = FontConfig::new("", 14.0);
        assert_eq!(cfg.effective_family(), PLATFORM_MONOSPACE);
    }

    #[test]
    fn nonempty_family_is_preserved() {
        let cfg = FontConfig::new("Fira Code", 14.0);
        assert_eq!(cfg.effective_family(), "Fira Code");
    }

    #[test]
    fn cell_display_text_preserves_zero_width_codepoints() {
        assert_eq!(
            cell_display_text('\u{2699}', Some(&['\u{fe0f}'])),
            "\u{2699}\u{fe0f}"
        );
        assert_eq!(cell_display_text('a', Some(&['\u{0301}'])), "a\u{0301}");
    }

    #[test]
    fn cell_display_text_normalizes_nul_to_space() {
        assert_eq!(cell_display_text('\0', None), " ");
    }

    #[test]
    fn foreground_named_color_uses_fg_override_when_present() {
        let fg = [0.12, 0.34, 0.56, 1.0];
        let resolved = resolve_fg_color(Some(fg), None, &AnsiColor::Named(NamedColor::Foreground));
        assert_eq!(resolved, fg);
    }

    #[test]
    fn named_ansi_color_uses_ansi_override_palette() {
        let mut palette = [[0.0, 0.0, 0.0, 1.0]; 16];
        palette[12] = [0.91, 0.22, 0.13, 1.0];
        let resolved = resolve_fg_color(
            None,
            Some(&palette),
            &AnsiColor::Named(NamedColor::BrightBlue),
        );
        assert_eq!(resolved, palette[12]);
    }

    #[test]
    fn viewport_cache_matches_only_same_offset_line_count_and_columns() {
        assert!(super::viewport_cache_matches(
            Some(3),
            Some(80),
            Some(5),
            80,
            3,
            5
        ));
        assert!(!super::viewport_cache_matches(
            Some(3),
            Some(81),
            Some(5),
            80,
            3,
            5
        ));
        assert!(!super::viewport_cache_matches(
            Some(3),
            Some(80),
            Some(6),
            80,
            3,
            5
        ));
        assert!(!super::viewport_cache_matches(
            Some(4),
            Some(80),
            Some(5),
            80,
            3,
            5
        ));
        assert!(!super::viewport_cache_matches(
            None,
            Some(80),
            Some(5),
            80,
            3,
            5
        ));
    }
}
