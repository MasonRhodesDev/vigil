//! The day-1 presenter (DESIGN.md §7): double-buffered DRM dumb buffers,
//! XRGB8888, handed to the renderer as plain byte slices via
//! [`vigil_core::FrameTarget`]. This path is permanent — it is the zero-GPU
//! baseline, not scaffolding for the GL presenter.

use smithay::backend::drm::{DrmSurface, PlaneConfig, PlaneState};
use smithay::reexports::drm::buffer::{Buffer as _, DrmFourcc};
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::{Device as ControlDevice, framebuffer};
use smithay::utils::{Rectangle, Transform};
use vigil_core::{Canvas, FrameTarget, PresentError, Presenter};

const BPP: u32 = 32;
const DEPTH: u32 = 24;

struct Slot {
    buffer: DumbBuffer,
    fb: framebuffer::Handle,
}

/// Software presenter over a pair of dumb buffers on one DRM surface.
///
/// The first `with_frame` performs the modeset commit; every later frame is
/// a page flip to the freshly drawn back buffer.
//
// TODO(M1 wiring): flips currently request no vblank event (`event: false`).
// Once the binary registers the DrmDeviceNotifier, pass `true` and gate the
// next flip on the vblank to avoid EBUSY under load.
pub struct DumbBufferPresenter {
    surface: DrmSurface,
    slots: [Slot; 2],
    back: usize,
    modeset_done: bool,
    width: u32,
    height: u32,
}

fn backend(e: impl std::fmt::Display) -> PresentError {
    PresentError::Backend(e.to_string())
}

impl DumbBufferPresenter {
    /// Allocate the swapchain for the surface's pending mode and take
    /// ownership of the surface.
    pub fn new(surface: DrmSurface) -> Result<Self, PresentError> {
        let mode = surface.pending_mode();
        let (width, height) = mode.size();
        let (width, height) = (width as u32, height as u32);

        let mk_slot = || -> Result<Slot, PresentError> {
            let buffer = surface
                .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, BPP)
                .map_err(backend)?;
            let fb = surface
                .add_framebuffer(&buffer, DEPTH, BPP)
                .map_err(backend)?;
            Ok(Slot { buffer, fb })
        };
        let slots = [mk_slot()?, mk_slot()?];

        Ok(Self {
            surface,
            slots,
            back: 0,
            modeset_done: false,
            width,
            height,
        })
    }

    fn plane_state(&self, fb: framebuffer::Handle) -> PlaneState<'_> {
        PlaneState {
            handle: self.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((self.width as f64, self.height as f64).into()),
                dst: Rectangle::from_size((self.width as i32, self.height as i32).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb,
                fence: None,
            }),
        }
    }
}

impl Presenter for DumbBufferPresenter {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn invalidate(&mut self) {
        self.modeset_done = false;
    }

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        let slot = &mut self.slots[self.back];

        let drew = {
            let stride = slot.buffer.pitch() as usize;
            let mut mapping = self
                .surface
                .map_dumb_buffer(&mut slot.buffer)
                .map_err(backend)?;
            draw(Canvas::Cpu(FrameTarget {
                buffer: mapping.as_mut(),
                width: self.width,
                height: self.height,
                stride,
            }))
        };
        if !drew {
            return Ok(false);
        }

        let fb = slot.fb;
        let state = self.plane_state(fb);
        let result = if self.modeset_done {
            self.surface.page_flip([state], false)
        } else {
            self.surface.commit([state], false)
        };
        result.map_err(|e| match e {
            smithay::backend::drm::DrmError::DeviceInactive => PresentError::DeviceLost,
            e => backend(e),
        })?;

        self.modeset_done = true;
        self.back ^= 1;
        Ok(true)
    }
}
