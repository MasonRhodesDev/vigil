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

fn present_error(e: smithay::backend::drm::DrmError) -> PresentError {
    use smithay::backend::drm::DrmError;
    use smithay::reexports::rustix;
    match e {
        DrmError::DeviceInactive => PresentError::DeviceLost,
        DrmError::Access(ref a)
            if matches!(
                rustix::io::Errno::from_io_error(&a.source),
                Some(
                    rustix::io::Errno::NODEV | rustix::io::Errno::ACCESS | rustix::io::Errno::PERM
                )
            ) =>
        {
            PresentError::DeviceLost
        }
        e => backend(e),
    }
}

fn plane_transform(transform: u8) -> Transform {
    match transform % 4 {
        1 => Transform::_90,
        2 => Transform::_180,
        3 => Transform::_270,
        _ => Transform::Normal,
    }
}

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

struct Frame {
    fb: GbmFramebuffer,
    _buffer: GbmBuffer,
}

struct CursorPlane {
    plane: plane::Handle,
    _claim: PlaneClaim,
    _buffer: DumbBuffer,
    fb: framebuffer::Handle,
    size: (u32, u32),
    pos: Option<(i32, i32)>,
    dirty: bool,
}

pub struct GbmPresenter {
    surface: DrmSurface,
    drm: DrmDeviceFd,
    gbm: GbmWindow,
    current: Option<Frame>,
    previous: Option<Frame>,
    cursor: Option<CursorPlane>,
    modeset_done: bool,
    flip_pending: bool,
    width: u32,
    height: u32,
    transform: u8,
    mode_size: (u32, u32),
}

impl GbmPresenter {
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
        let (width, height) = if transform % 2 == 1 {
            (mh, mw)
        } else {
            (mw, mh)
        };
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
                flip_pending: false,
                width,
                height,
                transform,
                mode_size: (mw, mh),
            },
            gl,
        ))
    }

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
        let (bw, bh) = if transform % 2 == 1 {
            (mh, mw)
        } else {
            (mw, mh)
        };
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

    fn find_cursor_plane(surface: &DrmSurface) -> Option<&smithay::backend::drm::PlaneInfo> {
        surface
            .planes()
            .cursor
            .iter()
            .find(|p| p.formats.iter().any(|f| f.code == DrmFourcc::Argb8888))
    }

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
            return None;
        }
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

    pub fn has_cursor(&self) -> bool {
        self.cursor.is_some()
    }

    fn cursor_state(&self) -> Option<PlaneState<'_>> {
        let cursor = self.cursor.as_ref()?;
        Some(PlaneState {
            handle: cursor.plane,
            config: cursor.pos.map(|pos| PlaneConfig {
                src: Rectangle::from_size(
                    (f64::from(cursor.size.0), f64::from(cursor.size.1)).into(),
                ),
                dst: Rectangle::new(
                    cursor_dst(self.transform, (self.width, self.height), cursor.size, pos).into(),
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
        self.flip_pending = false;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.dirty = true;
        }
    }

    fn vblank(&mut self) {
        self.flip_pending = false;
    }

    fn crtc_id(&self) -> Option<u32> {
        Some(self.surface.crtc().into())
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

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        if self.flip_pending {
            return Ok(false);
        }
        let drew = draw(Canvas::Gl {
            width: self.width,
            height: self.height,
        });
        if !drew {
            let cursor_dirty = self.cursor.as_ref().is_some_and(|c| c.dirty);
            let Some(current) = self.current.as_ref() else {
                return Ok(false);
            };
            if !(cursor_dirty && self.modeset_done) {
                return Ok(false);
            }
            let mut states = vec![self.plane_state(*current.fb.as_ref())];
            states.extend(self.cursor_state());
            self.surface
                .page_flip(states, true)
                .map_err(present_error)?;
            if let Some(cursor) = self.cursor.as_mut() {
                cursor.dirty = false;
            }
            self.flip_pending = true;
            return Ok(true);
        }

        let bo = unsafe { self.gbm.lock_front_buffer() }.map_err(backend)?;
        let buffer = GbmBuffer::from_bo(bo, true);
        let fb = framebuffer_from_bo(&self.drm, &buffer, false).map_err(backend)?;

        let mut states = vec![self.plane_state(*fb.as_ref())];
        states.extend(self.cursor_state());
        let result = if self.modeset_done {
            self.surface.page_flip(states, true)
        } else {
            self.surface.commit(states, true)
        };
        result.map_err(present_error)?;
        self.modeset_done = true;
        self.flip_pending = true;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.dirty = false;
        }

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
    fn plane_transform_matches_drm_quarter_turns() {
        assert_eq!(plane_transform(0), Transform::Normal);
        assert_eq!(plane_transform(1), Transform::_90);
        assert_eq!(plane_transform(2), Transform::_180);
        assert_eq!(plane_transform(3), Transform::_270);
        assert_eq!(plane_transform(5), Transform::_90);
    }

    #[test]
    fn cursor_dst_matches_software_blit() {
        assert_eq!(cursor_dst(0, (4, 6), (2, 2), (1, 2)), (1, 2));
        assert_eq!(cursor_dst(1, (4, 6), (2, 2), (1, 2)), (2, 1));
        assert_eq!(cursor_dst(3, (4, 6), (2, 2), (1, 2)), (2, 1));
        assert_eq!(cursor_dst(2, (4, 6), (2, 2), (1, 2)), (1, 2));
    }

    #[test]
    fn cursor_bitmap_rotates_within_square_buffer() {
        assert_eq!(vigil_core::scene_to_panel(1, 2, 2, 0, 0), (0, 1));
        assert_eq!(vigil_core::scene_to_panel(3, 2, 2, 0, 0), (1, 0));
    }
}
