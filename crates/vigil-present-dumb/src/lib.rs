//! The day-1 presenter (DESIGN.md §7): double-buffered DRM dumb buffers,
//! XRGB8888, handed to the renderer as plain byte slices via
//! [`vigil_core::FrameTarget`]. This path is permanent — it is the zero-GPU
//! baseline, not scaffolding for the GL presenter.

use vigil_core::{FrameTarget, PresentError, Presenter};

/// Software presenter over a pair of mapped dumb buffers on one DRM surface.
pub struct DumbBufferPresenter {
    _private: (),
}

impl DumbBufferPresenter {
    /// Allocate the swapchain for an output's active mode.
    pub fn new(_width: u32, _height: u32) -> Result<Self, PresentError> {
        todo!("M1: DumbAllocator swapchain + framebuffer attach")
    }
}

impl Presenter for DumbBufferPresenter {
    fn size(&self) -> (u32, u32) {
        todo!("M1")
    }

    fn with_frame(&mut self, _draw: &mut dyn FnMut(FrameTarget<'_>)) -> Result<(), PresentError> {
        todo!("M1: map back buffer, draw, page-flip")
    }
}
