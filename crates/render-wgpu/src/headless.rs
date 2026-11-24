//! Headless wgpu device helper for deterministic offscreen tests/examples.
//!
//! This deliberately does not create a window or surface. On machines without a
//! hardware GPU wgpu may select the software Vulkan (lavapipe) or GL adapter;
//! the selected adapter is reported so benchmark numbers can be interpreted
//! honestly.

use futures_lite::future::block_on;

/// An adapter/device/queue bundle without a window.
pub struct HeadlessGpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: wgpu::AdapterInfo,
}

impl std::fmt::Debug for HeadlessGpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeadlessGpu")
            .field("adapter", &self.info.name)
            .field("backend", &self.info.backend)
            .field("device_type", &self.info.device_type)
            .field("driver", &self.info.driver)
            .finish_non_exhaustive()
    }
}

/// Requests a backend adapter and device without a surface.
pub fn request_headless_gpu() -> Result<HeadlessGpu, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    }))
    .map_err(|error| {
        format!(
            "no headless wgpu adapter: {error}; install a Vulkan/GLES software \
             adapter or run under Xvfb"
        )
    })?;
    let info = adapter.get_info();
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("xtervo-wgpu-text-headless"),
        ..Default::default()
    }))
    .map_err(|error| format!("failed to request headless wgpu device: {error}"))?;
    Ok(HeadlessGpu {
        instance,
        adapter,
        device,
        queue,
        info,
    })
}
