//! Slint presenter for an offscreen WGPU texture.
//!
//! [`SlintTexturePresenter`] wraps a [`wgpu::Texture`] with
//! `slint::Image::try_from`, keeps the texture alive for as long as the image is
//! used, pushes the image into a Slint component property on every
//! [`present`](SlintTexturePresenter::present), and requests a window redraw.
//!
//! Only the winit + femtovg-WGPU Slint backend exposes the device/queue needed
//! to create a compatible texture. Use
//! `slint::Window::set_rendering_notifier()` with
//! `slint::RenderingState::RenderingSetup` and match
//! `slint::GraphicsAPI::WGPU30 { device, queue, .. }`. See the `two_colors`
//! example for the complete, headless-testable wiring.

use std::fmt;

use tervo_scene::PhysicalSize;

/// Errors from validating/importing a texture or updating the UI image.
#[derive(Debug)]
pub enum PresenterError {
    /// The texture has zero width or height.
    EmptyTexture { width: u32, height: u32 },
    /// The format is not one of the two formats accepted by Slint.
    UnsupportedFormat(wgpu::TextureFormat),
    /// A required texture usage flag is missing.
    MissingUsage(&'static str),
    /// Slint rejected the texture (should be unreachable after local validation).
    ImageImport(slint::wgpu_30::TextureImportError),
}

impl fmt::Display for PresenterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTexture { width, height } => {
                write!(f, "cannot import zero-sized texture {width}x{height}")
            }
            Self::UnsupportedFormat(format) => write!(
                f,
                "unsupported texture format {format:?}; Slint accepts Rgba8Unorm or Rgba8UnormSrgb"
            ),
            Self::MissingUsage(usage) => {
                write!(f, "texture is missing required usage {usage}")
            }
            Self::ImageImport(error) => {
                write!(f, "slint::Image::try_from rejected the texture: {error}")
            }
        }
    }
}

impl std::error::Error for PresenterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ImageImport(error) => Some(error),
            Self::EmptyTexture { .. } | Self::UnsupportedFormat(_) | Self::MissingUsage(_) => None,
        }
    }
}

fn validate_texture(
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
    width: u32,
    height: u32,
) -> Result<PhysicalSize, PresenterError> {
    if width == 0 || height == 0 {
        return Err(PresenterError::EmptyTexture { width, height });
    }
    if format != wgpu::TextureFormat::Rgba8Unorm && format != wgpu::TextureFormat::Rgba8UnormSrgb {
        return Err(PresenterError::UnsupportedFormat(format));
    }
    if !usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
        return Err(PresenterError::MissingUsage("TEXTURE_BINDING"));
    }
    if !usage.contains(wgpu::TextureUsages::RENDER_ATTACHMENT) {
        return Err(PresenterError::MissingUsage("RENDER_ATTACHMENT"));
    }
    Ok(PhysicalSize { width, height })
}

/// Holds an imported terminal texture and presents it into a Slint window.
///
/// The presenter is deliberately UI-framework-agnostic: it receives a closure
/// that copies the imported `slint::Image` into the component property. That
/// keeps this crate independent of any specific Slint component type:
///
/// ```ignore
/// let weak = ui.as_weak();
/// let presenter = SlintTexturePresenter::new(texture, move |image| {
///     if let Some(ui) = weak.upgrade() {
///         ui.set_frame(image);
///     }
/// })?;
/// ```
///
/// Call [`present`](Self::present) after the backend finished writing the
/// texture contents. It re-sets the image property (so UI bindings observe the
/// new generation) and calls `Window::request_redraw()`.
pub struct SlintTexturePresenter {
    texture: wgpu::Texture,
    image: slint::Image,
    size: PhysicalSize,
    revision: u64,
    apply_image: Box<dyn Fn(slint::Image)>,
}

impl SlintTexturePresenter {
    /// Imports `texture` via `slint::Image::try_from` and keeps it alive.
    ///
    /// The texture must satisfy the Slint import contract:
    /// - `Rgba8Unorm` or `Rgba8UnormSrgb`
    /// - `TEXTURE_BINDING | RENDER_ATTACHMENT`
    ///
    /// The texture must come from the same WGPU device Slint renders with.
    pub fn new(
        texture: wgpu::Texture,
        apply_image: impl Fn(slint::Image) + 'static,
    ) -> Result<Self, PresenterError> {
        let size = validate_texture(
            texture.format(),
            texture.usage(),
            texture.width(),
            texture.height(),
        )?;
        let image = slint::Image::try_from(texture.clone()).map_err(PresenterError::ImageImport)?;
        Ok(Self {
            texture,
            image,
            size,
            revision: 0,
            apply_image: Box::new(apply_image),
        })
    }

    /// The imported Slint image. It is backed by the live wgpu texture.
    pub fn image(&self) -> &slint::Image {
        &self.image
    }

    /// The texture kept alive by this presenter.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Size of the imported texture in physical pixels.
    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    /// Number of times [`Self::present`] has been called.
    ///
    /// This is a presenter-local counter, distinct from
    /// [`tervo_render::RenderedFrame::revision`].
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Swaps in a new texture, e.g. after a backend resize/recreation.
    ///
    /// The old texture handle is released. The UI property is not updated until
    /// the next [`Self::present`] call.
    pub fn replace_texture(&mut self, texture: wgpu::Texture) -> Result<(), PresenterError> {
        let size = validate_texture(
            texture.format(),
            texture.usage(),
            texture.width(),
            texture.height(),
        )?;
        let image = slint::Image::try_from(texture.clone()).map_err(PresenterError::ImageImport)?;
        self.texture = texture;
        self.image = image;
        self.size = size;
        Ok(())
    }

    /// Pushes the current image into the component and requests a redraw.
    ///
    /// The texture contents are expected to have been updated already through
    /// the same queue/device. No CPU copy happens here: the image still points
    /// at the GPU texture.
    pub fn present(&mut self, window: &slint::Window) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        (self.apply_image)(self.image.clone());
        window.request_redraw();
        self.revision
    }
}

impl fmt::Debug for SlintTexturePresenter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlintTexturePresenter")
            .field("size", &self.size)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMPORT_USAGE: wgpu::TextureUsages =
        wgpu::TextureUsages::TEXTURE_BINDING.union(wgpu::TextureUsages::RENDER_ATTACHMENT);

    #[test]
    fn accepts_the_two_supported_formats() {
        for format in [
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ] {
            assert_eq!(
                validate_texture(format, IMPORT_USAGE, 320, 200).unwrap(),
                PhysicalSize {
                    width: 320,
                    height: 200
                }
            );
        }
    }

    #[test]
    fn rejects_other_formats() {
        let err = validate_texture(wgpu::TextureFormat::Bgra8UnormSrgb, IMPORT_USAGE, 320, 200)
            .unwrap_err();
        assert!(matches!(err, PresenterError::UnsupportedFormat(_)));
    }

    #[test]
    fn rejects_missing_binding_usage() {
        let err = validate_texture(
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
            320,
            200,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PresenterError::MissingUsage("TEXTURE_BINDING")
        ));
    }

    #[test]
    fn rejects_missing_render_attachment_usage() {
        let err = validate_texture(
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::TEXTURE_BINDING,
            320,
            200,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            PresenterError::MissingUsage("RENDER_ATTACHMENT")
        ));
    }

    #[test]
    fn rejects_zero_sized_texture() {
        let err =
            validate_texture(wgpu::TextureFormat::Rgba8Unorm, IMPORT_USAGE, 0, 200).unwrap_err();
        assert!(matches!(
            err,
            PresenterError::EmptyTexture {
                width: 0,
                height: 200
            }
        ));
    }

    #[test]
    fn error_display_is_actionable() {
        let err = PresenterError::MissingUsage("TEXTURE_BINDING");
        assert!(err.to_string().contains("TEXTURE_BINDING"));
        let err = PresenterError::UnsupportedFormat(wgpu::TextureFormat::Rgba16Float);
        assert!(err.to_string().contains("Rgba8Unorm"));
    }
}
