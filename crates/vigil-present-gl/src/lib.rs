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
use vigil_core::{Canvas, PresentError, Presenter, scene_to_panel};
use vigil_gl::{GbmWindow, GlContext, GlSurface};

fn backend(e: impl std::fmt::Display) -> PresentError {
    PresentError::Backend(e.to_string())
}

/// A device that vanished (ENODEV) is as gone as one that got paused:
/// surprise removal — a dock GPU unplugged at the greeter (#6) — must drop
/// the output, not retry the present forever.
fn present_error(e: smithay::backend::drm::DrmError) -> PresentError {
    use smithay::backend::drm::DrmError;
    use smithay::reexports::rustix;
    match e {
        DrmError::DeviceInactive => PresentError::DeviceLost,
        DrmError::Access(ref a)
            if rustix::io::Errno::from_io_error(&a.source) == Some(rustix::io::Errno::NODEV) =>
        {
            PresentError::DeviceLost
        }
        e => backend(e),
    }
}

/// vigil transform (wl_output-style: how the panel is mounted; the scene is
/// drawn rotated the OPPOSITE way — T=1 puts the scene on the panel turned
/// 90° clockwise, see `vigil_core::scene_to_panel`) → the DRM rotation that
/// reproduces it. The DRM `rotation` property is counter-clockwise, so the
/// quarter turns invert: clockwise-90 needs ROTATE_270 and vice versa.
fn plane_transform(transform: u8) -> Transform {
    match transform % 4 {
        1 => Transform::_270,
        2 => Transform::_180,
        3 => Transform::_90,
        _ => Transform::Normal,
    }
}

/// Where the cursor plane goes for a pointer at scene `pos`: the pointer's
/// panel position, offset so the rotated buffer's hotspot pixel (the arrow
/// tip, bitmap (0,0)) lands exactly there. Derivation mirrors the software
/// path's blit_cursor pixel-for-pixel; see cursor_dst_matches_software_blit.
fn cursor_dst(transform: u8, scene: (u32, u32), plane: (u32, u32), pos: (i32, i32)) -> (i32, i32) {
    let (sw, sh) = (scene.0 as usize, scene.1 as usize);
    let (sx, sy) = (
        (pos.0.max(0) as usize).min(sw - 1),
        (pos.1.max(0) as usize).min(sh - 1),
    );
    let p = scene_to_panel(transform, sw, sh, sx, sy);
    let h = scene_to_panel(transform, plane.0 as usize, plane.1 as usize, 0, 0);
    (p.0 as i32 - h.0 as i32, p.1 as i32 - h.1 as i32)
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
    /// `Some(scene position)` = shown there (mapped to panel coordinates
    /// at submission); `None` = hidden.
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
    /// Buffer/scene dimensions — what GL renders at. Swapped from the mode
    /// on quarter turns; the plane rotates the buffer onto the panel (#26).
    width: u32,
    height: u32,
    /// wl_output-style transform (0..=3 after normalization).
    transform: u8,
    /// Panel/CRTC dimensions, straight from the mode.
    mode_size: (u32, u32),
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
        transform: u8,
    ) -> Result<(Self, GlSurface), PresentError> {
        let transform = transform % 4;
        let mode = surface.pending_mode();
        let (mw, mh) = mode.size();
        let (mw, mh) = (mw as u32, mh as u32);
        // The GL scene is rendered upright at scene dims; the plane rotates
        // it onto the panel (#26). Quarter turns swap the buffer's aspect.
        let (width, height) = if transform % 2 == 1 { (mh, mw) } else { (mw, mh) };
        let gl = GlSurface::new(context, width, height).map_err(backend)?;
        let gbm = gl.window();
        let cursor = Self::cursor_plane(&surface, cursor_scale, transform);
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
                transform,
                mode_size: (mw, mh),
            },
            gl,
        ))
    }

    /// Whether [`Self::new`] would find a usable cursor plane on this
    /// surface at this transform.
    ///
    /// For callers deciding between renderers BEFORE handing the surface
    /// over — construction consumes it, so a policy of "no plane, no GL"
    /// must be able to ask first and keep the surface for software. A
    /// quarter-turned output needs a square cursor buffer (the bitmap is
    /// pre-rotated in place, #26); a non-square plane on odd transforms
    /// reads as unusable.
    pub fn probe_cursor(surface: &DrmSurface, transform: u8) -> bool {
        let Some(info) = Self::find_cursor_plane(surface) else {
            return false;
        };
        if transform.is_multiple_of(2) {
            return true;
        }
        let (pw, ph) = Self::cursor_plane_size(info);
        pw == ph
    }

    /// Ask the display hardware whether it will scan out a rotated buffer,
    /// BEFORE any GL state is built: a TEST_ONLY atomic commit of a
    /// scanout-class GBM buffer at scene dims with the rotation set. Runs on
    /// `&DrmSurface`, so a refusal leaves the surface free for the software
    /// fallback (same pre-consumption contract as `probe_cursor`, #25).
    /// virtio-gpu has no rotation property at all — smithay then refuses to
    /// even build the request (UnknownProperty), which is exactly the signal.
    pub fn test_transform(
        surface: &DrmSurface,
        drm: &DrmDeviceFd,
        context: &GlContext,
        transform: u8,
    ) -> Result<(), String> {
        let transform = transform % 4;
        if transform == 0 {
            return Ok(());
        }
        let mode = surface.pending_mode();
        let (mw, mh) = mode.size();
        let (mw, mh) = (mw as u32, mh as u32);
        let (bw, bh) = if transform % 2 == 1 { (mh, mw) } else { (mw, mh) };
        let bo = context
            .gbm_device()
            .create_buffer_object::<()>(
                bw,
                bh,
                vigil_gl::GbmFormat::Xrgb8888,
                vigil_gl::GbmBufferFlags::SCANOUT | vigil_gl::GbmBufferFlags::RENDERING,
            )
            .map_err(|e| format!("test buffer: {e}"))?;
        let buffer = GbmBuffer::from_bo(bo, true);
        let fb = framebuffer_from_bo(drm, &buffer, false).map_err(|e| format!("test fb: {e}"))?;
        let state = PlaneState {
            handle: surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((bw as f64, bh as f64).into()),
                dst: Rectangle::from_size((mw as i32, mh as i32).into()),
                transform: plane_transform(transform),
                alpha: 1.0,
                damage_clips: None,
                fb: *fb.as_ref(),
                fence: None,
            }),
        };
        surface
            .test_state([state], true)
            .map_err(|e| format!("rotation refused: {e}"))
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
    fn cursor_plane_size(info: &smithay::backend::drm::PlaneInfo) -> (u32, u32) {
        info.size_hints
            .as_deref()
            .and_then(<[_]>::first)
            .map_or((64, 64), |s| (u32::from(s.w), u32::from(s.h)))
    }

    fn cursor_plane(surface: &DrmSurface, scale: f32, transform: u8) -> Option<CursorPlane> {
        let info = Self::find_cursor_plane(surface)?;
        let (pw, ph) = Self::cursor_plane_size(info);
        if transform % 2 == 1 && pw != ph {
            // probe_cursor refused this combination; construction agrees.
            return None;
        }
        // The bitmap must fit the plane; a HiDPI factor that would overflow
        // it is clamped rather than clipped.
        let max_scale = (pw as f32 / vigil_core::CURSOR[0].len() as f32)
            .min(ph as f32 / vigil_core::CURSOR.len() as f32);
        let (argb, cw, ch) = vigil_core::cursor_argb(scale.min(max_scale));
        let claim = surface.claim_plane(info.handle)?;
        let mut buffer = surface
            .create_dumb_buffer((pw, ph), DrmFourcc::Argb8888, 32)
            .ok()?;
        // The scene rotates onto the panel, so the cursor bitmap must make
        // the same trip inside its own (square, for odd transforms) plane
        // buffer — the software path does this per-pixel in blit_cursor;
        // here it happens once at construction.
        {
            let pitch = buffer.pitch() as usize;
            let mut mapping = surface.map_dumb_buffer(&mut buffer).ok()?;
            let bytes = mapping.as_mut();
            bytes.fill(0);
            for y in 0..ch as usize {
                for x in 0..cw as usize {
                    let src = (y * cw as usize + x) * 4;
                    let (qx, qy) = scene_to_panel(transform, pw as usize, ph as usize, x, y);
                    bytes[qy * pitch + qx * 4..qy * pitch + qx * 4 + 4]
                        .copy_from_slice(&argb[src..src + 4]);
                }
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
            config: cursor.pos.map(|pos| PlaneConfig {
                src: Rectangle::from_size(
                    (f64::from(cursor.size.0), f64::from(cursor.size.1)).into(),
                ),
                // pos is scene coordinates; the plane goes at its panel
                // mapping. The cursor plane itself stays untransformed —
                // the bitmap was pre-rotated at construction instead,
                // which works on cursor planes with no rotation property.
                dst: Rectangle::new(
                    cursor_dst(
                        self.transform,
                        (self.width, self.height),
                        cursor.size,
                        pos,
                    )
                    .into(),
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
                dst: Rectangle::from_size(
                    (self.mode_size.0 as i32, self.mode_size.1 as i32).into(),
                ),
                transform: plane_transform(self.transform),
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
            self.surface.page_flip(states, true).map_err(present_error)?;
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
        result.map_err(present_error)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_transform_inverts_quarter_turns() {
        assert_eq!(plane_transform(0), Transform::Normal);
        assert_eq!(plane_transform(1), Transform::_270); // scene drawn CW-90 → DRM CCW-270
        assert_eq!(plane_transform(2), Transform::_180);
        assert_eq!(plane_transform(3), Transform::_90);
        assert_eq!(plane_transform(5), Transform::_270); // flipped variants rotate-without-flip
    }

    #[test]
    fn cursor_dst_matches_software_blit() {
        // Scene 4x6 (sw=4, sh=6), 2x2 cursor plane, pointer at scene (1,2).
        // Software puts the tip at scene_to_panel(T, 4, 6, 1, 2); the plane
        // dst is that point minus the rotated hotspot. Literals by hand:
        assert_eq!(cursor_dst(0, (4, 6), (2, 2), (1, 2)), (1, 2));
        assert_eq!(cursor_dst(1, (4, 6), (2, 2), (1, 2)), (2, 1)); // P=(3,1), H=(1,0)
        assert_eq!(cursor_dst(3, (4, 6), (2, 2), (1, 2)), (2, 1)); // P=(2,2), H=(0,1)
        assert_eq!(cursor_dst(2, (4, 6), (2, 2), (1, 2)), (1, 2)); // P=(2,3), H=(1,1)
    }

    #[test]
    fn cursor_bitmap_rotates_within_square_buffer() {
        // A 2x2 "bitmap" in a 2x2 buffer: pixel (0,0) must land where
        // scene_to_panel sends it (T=1 → (1,0); T=3 → (0,1)).
        assert_eq!(vigil_core::scene_to_panel(1, 2, 2, 0, 0), (1, 0));
        assert_eq!(vigil_core::scene_to_panel(3, 2, 2, 0, 0), (0, 1));
    }
}
