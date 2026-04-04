//! Winit 0.30 window with wgpu surface for Therminal.
//!
//! Implements the `ApplicationHandler` trait to create a cross-platform window
//! (Wayland, X11, macOS Cocoa, Windows Win32) with a GPU-accelerated surface
//! suitable for terminal rendering.

use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::Key;
use winit::window::{Window, WindowId};

/// Background color: thermal dark (#0a0010).
const BG_COLOR: wgpu::Color = wgpu::Color {
    r: 10.0 / 255.0,
    g: 0.0,
    b: 16.0 / 255.0,
    a: 1.0,
};

/// Holds the wgpu state created after the window surface is available.
struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

/// Main application struct implementing winit's `ApplicationHandler`.
pub struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
}

impl App {
    pub fn new() -> Self {
        Self {
            window: None,
            gpu: None,
        }
    }

    /// Initialize wgpu for the given window. Called once on first `resumed`.
    fn init_gpu(&mut self, window: Arc<Window>) {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // SAFETY: surface lifetime is tied to the Arc<Window> we hold.
        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no suitable GPU adapter found");

        info!(
            "wgpu adapter: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("therminal"),
                ..Default::default()
            },
            None,
        ))
        .expect("failed to create wgpu device");

        let surface_caps = surface.get_capabilities(&adapter);
        let format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        self.window = Some(window);
        self.gpu = Some(GpuState {
            surface,
            device,
            queue,
            config,
        });
    }

    /// Resize the surface to match the new window size.
    fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.config.width = new_size.width;
            gpu.config.height = new_size.height;
            gpu.surface.configure(&gpu.device, &gpu.config);
            debug!("surface resized to {}x{}", new_size.width, new_size.height);
        }
    }

    /// Render a single frame (clear to background color).
    fn render(&mut self) {
        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };

        let output = match gpu.surface.get_current_texture() {
            Ok(tex) => tex,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                // Reconfigure and retry next frame.
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                warn!("wgpu: out of memory");
                return;
            }
            Err(e) => {
                warn!("wgpu surface error: {e}");
                return;
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("therminal_clear"),
            });

        // Clear to thermal background color.
        let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(BG_COLOR),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        drop(_render_pass);

        gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // Already initialized (e.g. re-resume on Android).
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("Therminal")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );

        let scale = window.scale_factor();
        info!("window created (scale_factor={scale:.2})");

        self.init_gpu(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                info!("close requested, exiting");
                event_loop.exit();
            }

            WindowEvent::Resized(new_size) => {
                self.resize(new_size);
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                info!("scale factor changed to {scale_factor:.2}");
                let new_size = self.window.as_ref().map(|w| w.inner_size());
                if let Some(size) = new_size {
                    self.resize(size);
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                self.render();
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                debug!("key pressed: {logical_key:?}");
                match logical_key {
                    Key::Named(winit::keyboard::NamedKey::Escape) => {
                        info!("escape pressed, exiting");
                        event_loop.exit();
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }
}

/// Create the event loop, set control flow to Wait, and run the app.
pub fn run() -> Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::new();
    event_loop.run_app(&mut app)?;

    Ok(())
}
