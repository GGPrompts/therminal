use tracing::info;
use wgpu::{Adapter, Device, Instance, Queue};

pub struct WgpuContext {
    pub instance: Instance,
    pub adapter: Adapter,
    pub device: Device,
    pub queue: Queue,
}

impl WgpuContext {
    /// Synchronously create a WgpuContext with high-performance defaults suitable
    /// for all therminal components. Uses pollster::block_on internally so callers
    /// do not need an async runtime. Panics if no adapter is found.
    pub fn new() -> Self {
        let backends = if cfg!(target_os = "linux") {
            wgpu::Backends::VULKAN
        } else {
            wgpu::Backends::default()
        };
        info!("wgpu backends: {:?}", backends);
        let instance = Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("therminal-core: no wgpu adapter found");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .expect("therminal-core: failed to create wgpu device");
        Self {
            instance,
            adapter,
            device,
            queue,
        }
    }
}

impl Default for WgpuContext {
    fn default() -> Self {
        Self::new()
    }
}
