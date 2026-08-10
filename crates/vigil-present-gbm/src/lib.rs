//! The GL presenter (DESIGN.md §7, issue #17): femtovg renders into a GBM
//! surface and the swapped buffer is scanned out on a DRM CRTC.
//!
//! Software rendering remains the baseline; this is fidelity headroom, and
//! the greeter only links it when built with the `gl` feature.
//!
//! # Buffer lifecycle
//!
//! A GBM surface owns a small, fixed pool of buffers. `lock_front_buffer`
//! takes one out of that pool and it stays out until released, so a presenter
//! that never releases runs the pool dry after a couple of frames and then
//! fails -- the classic version of this bug looks perfect in a one-frame demo.
//!
//! Release cannot be immediate either: the buffer just flipped to is the one
//! the CRTC is scanning out. So one generation is held back. When frame N's
//! flip is submitted, frame N-1's buffer is still on screen and frame N-2's
//! is provably free, and that is the one released.

use std::rc::Rc;

use smithay::backend::allocator::gbm::GbmBuffer;
use smithay::backend::drm::gbm::{GbmFramebuffer, framebuffer_from_bo};
use smithay::backend::drm::{DrmDeviceFd, DrmSurface, PlaneConfig, PlaneState};
use smithay::utils::{Rectangle, Transform};
use vigil_core::{Canvas, PresentError, Presenter};
use vigil_gl::{GbmWindow, GlContext, GlSurface};

fn backend(e: impl std::fmt::Display) -> PresentError {
    PresentError::Backend(e.to_string())
}

/// One frame's worth of scanout state: the buffer and the framebuffer made
/// from it. The framebuffer must outlive the flip, and the buffer must
/// outlive the framebuffer, so they travel together.
struct Frame {
    /// Declared first so it drops first: the DRM framebuffer must go before
    /// the buffer it was made from.
    _fb: GbmFramebuffer,
    /// Dropping this calls gbm_surface_release_buffer, returning the buffer
    /// to the surface's pool.
    _buffer: GbmBuffer,
}

pub struct GbmPresenter {
    surface: DrmSurface,
    drm: DrmDeviceFd,
    gbm: GbmWindow,
    /// Scanning out now.
    current: Option<Frame>,
    /// Scanned out until `current` took over; released when the next flip is
    /// submitted (see the lifecycle note above).
    previous: Option<Frame>,
    modeset_done: bool,
    width: u32,
    height: u32,
}

impl GbmPresenter {
    /// Build a presenter for `surface`, rendering through `context`.
    ///
    /// `drm` must be the device the surface belongs to -- framebuffers are
    /// created against it.
    pub fn new(
        surface: DrmSurface,
        drm: DrmDeviceFd,
        context: Rc<GlContext>,
    ) -> Result<(Self, GlSurface), PresentError> {
        let mode = surface.pending_mode();
        let (width, height) = mode.size();
        let (width, height) = (width as u32, height as u32);
        let gl = GlSurface::new(context, width, height).map_err(backend)?;
        let gbm = gl.window();
        Ok((
            Self {
                surface,
                drm,
                gbm,
                current: None,
                previous: None,
                modeset_done: false,
                width,
                height,
            },
            gl,
        ))
    }

    fn plane_state(
        &self,
        fb: smithay::reexports::drm::control::framebuffer::Handle,
    ) -> PlaneState<'_> {
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

impl Presenter for GbmPresenter {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn invalidate(&mut self) {
        self.modeset_done = false;
    }

    /// Draw and submit one frame.
    ///
    /// On `Ok(true)` a flip is in flight: the caller must wait for the DRM
    /// page-flip event before calling again, or the next submission is
    /// refused with EBUSY.
    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        // The renderer draws with GL and swaps as part of presenting, so by
        // the time this returns the front buffer is the frame just drawn.
        let drew = draw(Canvas::Gl {
            width: self.width,
            height: self.height,
        });
        if !drew {
            return Ok(false);
        }

        // SAFETY: a swap just happened, and the buffer is released below once
        // it is provably off screen.
        let bo = unsafe { self.gbm.lock_front_buffer() }.map_err(backend)?;
        let buffer = GbmBuffer::from_bo(bo, true);
        let fb = framebuffer_from_bo(&self.drm, &buffer, false).map_err(backend)?;

        // Request the vblank event (`true`). A flip submitted while the
        // previous one is still pending is refused with EBUSY, so the caller
        // must wait for the completion event before presenting again --
        // [`Self::with_frame`] documents that contract.
        let state = self.plane_state(*fb.as_ref());
        let result = if self.modeset_done {
            self.surface.page_flip([state], true)
        } else {
            self.surface.commit([state], true)
        };
        result.map_err(|e| match e {
            smithay::backend::drm::DrmError::DeviceInactive => PresentError::DeviceLost,
            e => backend(e),
        })?;
        self.modeset_done = true;

        // Two generations back is off screen for certain; dropping it returns
        // its buffer to the surface's pool.
        self.previous = self.current.take();
        self.current = Some(Frame {
            _fb: fb,
            _buffer: buffer,
        });
        Ok(true)
    }
}
