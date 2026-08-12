//! The GL presenter (DESIGN.md §2, §7, issue #17): femtovg renders into a
//! GBM surface and the swapped buffer is scanned out on a DRM CRTC.
//!
//! This is the crate the workspace reserved for M3. GL/GBM/EGL live here and
//! in vigil-gl, and nowhere else.
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
use smithay::backend::drm::{DrmDeviceFd, DrmSurface, PlaneClaim, PlaneConfig, PlaneState};
use smithay::reexports::drm::buffer::{Buffer as _, DrmFourcc};
use smithay::reexports::drm::control::{
    Device as ControlDevice, dumbbuffer::DumbBuffer, framebuffer, plane,
};
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
    fb: GbmFramebuffer,
    /// Dropping this calls gbm_surface_release_buffer, returning the buffer
    /// to the surface's pool.
    _buffer: GbmBuffer,
}

/// The DRM cursor plane and the one ARGB8888 buffer ever shown on it.
struct CursorPlane {
    plane: plane::Handle,
    /// Held so no other CRTC takes the plane while we use it.
    _claim: PlaneClaim,
    /// Kept alive for the framebuffer; never re-drawn after construction.
    _buffer: DumbBuffer,
    fb: framebuffer::Handle,
    /// Cursor buffer dimensions (the plane's preferred size).
    size: (u32, u32),
    /// `Some(panel position)` = shown there; `None` = hidden.
    pos: Option<(i32, i32)>,
    /// The plane state changed and the next submission must carry it.
    dirty: bool,
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
    cursor: Option<CursorPlane>,
    modeset_done: bool,
    width: u32,
    height: u32,
}

impl GbmPresenter {
    /// Build a presenter for `surface`, rendering through `context`.
    ///
    /// `drm` must be the device the surface belongs to -- framebuffers are
    /// created against it. `cursor_scale` is the HiDPI factor the cursor
    /// bitmap is rasterized at; clamped so the bitmap fits the plane's
    /// preferred size.
    pub fn new(
        surface: DrmSurface,
        drm: DrmDeviceFd,
        context: Rc<GlContext>,
        cursor_scale: f32,
    ) -> Result<(Self, GlSurface), PresentError> {
        let mode = surface.pending_mode();
        let (width, height) = mode.size();
        let (width, height) = (width as u32, height as u32);
        let gl = GlSurface::new(context, width, height).map_err(backend)?;
        let gbm = gl.window();
        let cursor = Self::cursor_plane(&surface, cursor_scale);
        Ok((
            Self {
                surface,
                drm,
                gbm,
                current: None,
                previous: None,
                cursor,
                modeset_done: false,
                width,
                height,
            },
            gl,
        ))
    }

    /// Whether [`Self::new`] would find a cursor plane on this surface.
    ///
    /// For callers deciding between renderers BEFORE handing the surface
    /// over — construction consumes it, so a policy of "no plane, no GL"
    /// must be able to ask first and keep the surface for software.
    pub fn probe_cursor(surface: &DrmSurface) -> bool {
        Self::find_cursor_plane(surface).is_some()
    }

    fn find_cursor_plane(
        surface: &DrmSurface,
    ) -> Option<&smithay::backend::drm::PlaneInfo> {
        surface
            .planes()
            .cursor
            .iter()
            .find(|p| p.formats.iter().any(|f| f.code == DrmFourcc::Argb8888))
    }

    /// Find, claim and fill an ARGB8888 cursor plane for this surface.
    /// `None` (with a log line left to the caller's policy) when the CRTC
    /// has no such plane — virtio-gpu hides it from clients that did not
    /// declare hotspot support, and legacy devices never reach here.
    fn cursor_plane(surface: &DrmSurface, scale: f32) -> Option<CursorPlane> {
        let info = Self::find_cursor_plane(surface)?;
        let (pw, ph) = info
            .size_hints
            .as_deref()
            .and_then(<[_]>::first)
            .map_or((64, 64), |s| (u32::from(s.w), u32::from(s.h)));
        // The bitmap must fit the plane; a HiDPI factor that would overflow
        // it is clamped rather than clipped.
        let max_scale = (pw as f32 / vigil_core::CURSOR[0].len() as f32)
            .min(ph as f32 / vigil_core::CURSOR.len() as f32);
        let (argb, cw, ch) = vigil_core::cursor_argb(scale.min(max_scale));
        let claim = surface.claim_plane(info.handle)?;
        let mut buffer = surface
            .create_dumb_buffer((pw, ph), DrmFourcc::Argb8888, 32)
            .ok()?;
        {
            let pitch = buffer.pitch() as usize;
            let mut mapping = surface.map_dumb_buffer(&mut buffer).ok()?;
            let bytes = mapping.as_mut();
            bytes.fill(0);
            for y in 0..ch as usize {
                let src = &argb[y * cw as usize * 4..(y + 1) * cw as usize * 4];
                bytes[y * pitch..y * pitch + src.len()].copy_from_slice(src);
            }
        }
        let fb = surface.add_framebuffer(&buffer, 32, 32).ok()?;
        Some(CursorPlane {
            plane: info.handle,
            _claim: claim,
            _buffer: buffer,
            fb,
            size: (pw, ph),
            pos: None,
            dirty: false,
        })
    }

    /// Whether a cursor plane was claimed (the binary's GL policy requires
    /// one; the examples do not care).
    pub fn has_cursor(&self) -> bool {
        self.cursor.is_some()
    }

    /// This frame's cursor plane state, when there is a cursor plane. A
    /// hidden cursor is an explicit `config: None`, which disables the
    /// plane — included in every submission so a modeset re-establishes
    /// the whole CRTC state.
    fn cursor_state(&self) -> Option<PlaneState<'_>> {
        let cursor = self.cursor.as_ref()?;
        Some(PlaneState {
            handle: cursor.plane,
            config: cursor.pos.map(|(x, y)| PlaneConfig {
                src: Rectangle::from_size(
                    (f64::from(cursor.size.0), f64::from(cursor.size.1)).into(),
                ),
                dst: Rectangle::new(
                    (x, y).into(),
                    (cursor.size.0 as i32, cursor.size.1 as i32).into(),
                ),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: cursor.fb,
                fence: None,
            }),
        })
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
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.dirty = true;
        }
    }

    fn set_cursor(&mut self, pos: Option<(i32, i32)>) -> bool {
        let Some(cursor) = self.cursor.as_mut() else {
            return false;
        };
        if cursor.pos != pos {
            cursor.pos = pos;
            cursor.dirty = true;
        }
        true
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
            // The scene is clean but the pointer may have moved: flip the
            // framebuffer already on screen with the cursor plane updated.
            // This is the whole point of the plane — pointer motion costs
            // one atomic flip, not a re-render (#25).
            let cursor_dirty = self.cursor.as_ref().is_some_and(|c| c.dirty);
            let Some(current) = self.current.as_ref() else {
                return Ok(false);
            };
            if !(cursor_dirty && self.modeset_done) {
                return Ok(false);
            }
            let mut states = vec![self.plane_state(*current.fb.as_ref())];
            states.extend(self.cursor_state());
            self.surface.page_flip(states, true).map_err(|e| match e {
                smithay::backend::drm::DrmError::DeviceInactive => PresentError::DeviceLost,
                e => backend(e),
            })?;
            if let Some(cursor) = self.cursor.as_mut() {
                cursor.dirty = false;
            }
            return Ok(true);
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
        let mut states = vec![self.plane_state(*fb.as_ref())];
        states.extend(self.cursor_state());
        let result = if self.modeset_done {
            self.surface.page_flip(states, true)
        } else {
            self.surface.commit(states, true)
        };
        result.map_err(|e| match e {
            smithay::backend::drm::DrmError::DeviceInactive => PresentError::DeviceLost,
            e => backend(e),
        })?;
        self.modeset_done = true;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.dirty = false;
        }

        // Two generations back is off screen for certain; dropping it returns
        // its buffer to the surface's pool.
        self.previous = self.current.take();
        self.current = Some(Frame {
            fb,
            _buffer: buffer,
        });
        Ok(true)
    }
}
