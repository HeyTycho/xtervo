//! Renderer boundary. Backends live in separate crates and consume `xtervo-scene`.

use xtervo_scene::{DamageRegion, PhysicalSize, Scene};

#[derive(Clone, Debug)]
pub struct RenderedFrame {
    pub revision: u64,
    pub size: PhysicalSize,
}

pub trait Renderer {
    type Error;
    type Frame;

    fn resize(&mut self, size: PhysicalSize) -> Result<(), Self::Error>;
    fn render(&mut self, scene: &Scene, damage: &DamageRegion) -> Result<Self::Frame, Self::Error>;
    fn invalidate(&mut self, damage: &DamageRegion);
    fn device_lost(&mut self);
}
