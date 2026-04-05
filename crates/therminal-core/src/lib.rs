//! Color palette, wgpu context, text renderer, font discovery, config, and geometry primitives.

pub mod config;
pub mod config_watcher;
pub mod font;
pub mod geometry;
pub mod palette;
pub mod text;
pub mod wgpu_ctx;

pub use therminal_protocol as protocol;
