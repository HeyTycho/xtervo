// SPDX-License-Identifier: GPL-3.0-only
//! Slint Image presentation contract for the terminal framebuffer.
//!
//! The app presents a double-buffered `SharedPixelBuffer` and reuses the older
//! buffer two frames later. This test drives Slint's partial renderer with
//! `SwappedBuffers` (the same buffer-age mode the winit software backend uses)
//! and proves that re-assigning an `Image` whose buffer was mutated in place is
//! re-rasterized. It guards against the "texture cache keyed by stale buffer
//! pointer" hypothesis: if this ever regresses, a per-frame copy workaround
//! would be required; otherwise the copy is pure overhead.

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_renderer_software::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::PlatformError;
use slint::{Image, PhysicalSize, Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};

slint::slint! {
    export component Probe inherits Window {
        width: 64px;
        height: 64px;
        in-out property <image> frame;
        Image {
            source: root.frame;
            width: 100%;
            height: 100%;
        }
    }
}

thread_local! {
    static PLATFORM_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct ProbePlatform;

impl slint::platform::Platform for ProbePlatform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::SwappedBuffers);
        PLATFORM_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone()));
        Ok(window)
    }
}

fn fill(color: [u8; 4]) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(64, 64);
    for pixel in bytemuck::cast_slice_mut::<_, [u8; 4]>(buffer.make_mut_bytes()) {
        *pixel = color;
    }
    buffer
}

fn render(window: &MinimalSoftwareWindow, target: &mut [Rgb8Pixel]) -> (u8, u8, u8) {
    window.draw_if_needed(|renderer| {
        renderer.render(target, 64);
    });
    let p = target[32 * 64 + 32];
    (p.r, p.g, p.b)
}

#[test]
fn same_pointer_reassigned_image_is_rerasterized_with_partial_rendering() {
    slint::platform::set_platform(Box::new(ProbePlatform)).unwrap();

    let ui = Probe::new().unwrap();
    ui.show().unwrap();
    let window = PLATFORM_WINDOW.with(|slot| slot.borrow().clone().unwrap());
    window.set_size(PhysicalSize::new(64, 64));

    let red = [255u8, 0, 0, 255];
    let blue = [0u8, 0, 255, 255];
    let green = [0u8, 255, 0, 255];
    let mut red_buffer = fill(red);
    let blue_buffer = fill(blue);

    let mut buffer0 = vec![Rgb8Pixel { r: 0, g: 0, b: 0 }; 64 * 64];
    let mut buffer1 = vec![Rgb8Pixel { r: 0, g: 0, b: 0 }; 64 * 64];

    ui.set_frame(Image::from_rgba8(red_buffer.clone()));
    window.request_redraw();
    assert_eq!(render(&window, &mut buffer0), (255, 0, 0), "frame 1 red");

    ui.set_frame(Image::from_rgba8(blue_buffer));
    window.request_redraw();
    assert_eq!(render(&window, &mut buffer1), (0, 0, 255), "frame 2 blue");

    // Frame 3 renders into buffer0 (the swap buffer from two frames ago).
    // Reuse the red buffer, but mutate it to green in place: the app's
    // double-buffer + fresh-Image pattern.
    for pixel in bytemuck::cast_slice_mut::<_, [u8; 4]>(red_buffer.make_mut_bytes()) {
        *pixel = green;
    }
    ui.set_frame(Image::from_rgba8(red_buffer.clone()));
    window.request_redraw();
    assert_eq!(
        render(&window, &mut buffer0),
        (0, 255, 0),
        "same-pointer re-assignment must repaint the stale swapped buffer"
    );
}
