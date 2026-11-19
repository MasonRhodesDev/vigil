#![deny(unsafe_code)]

use std::rc::Rc;

use smithay::backend::allocator::gbm::GbmBuffer;
use smithay::backend::drm::gbm::{GbmFramebuffer, framebuffer_from_bo};
use smithay::backend::drm::{DrmDeviceFd, DrmSurface, PlaneConfig, PlaneState};
use smithay::utils::Rectangle;
use vigil_core::{Canvas, PresentError, Presenter};
use vigil_gl::{GbmWindow, GlContext, GlSurface};
use vigil_present_kms::{CursorPlane, backend, plane_transform, present_error};

struct Frame {
    fb: GbmFramebuffer,
    _buffer: GbmBuffer,
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
        let cursor = CursorPlane::claim(&surface, cursor_scale, transform, (width, height)).ok();
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
        CursorPlane::probe(surface, transform).is_ok()
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
            cursor.invalidate();
        }
    }

    fn vblank(&mut self) {
        self.flip_pending = false;
    }

    fn crtc_id(&self) -> Option<u32> {
        Some(self.surface.crtc().into())
    }

    fn has_cursor(&self) -> bool {
        self.cursor.is_some()
    }

    fn set_cursor(&mut self, pos: Option<(i32, i32)>) -> bool {
        let Some(cursor) = self.cursor.as_mut() else {
            return false;
        };
        cursor.set(pos);
        true
    }

    #[allow(unsafe_code)]
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
            let cursor_dirty = self.cursor.as_ref().is_some_and(CursorPlane::is_dirty);
            let Some(current) = self.current.as_ref() else {
                return Ok(false);
            };
            if !(cursor_dirty && self.modeset_done) {
                return Ok(false);
            }
            let mut states = vec![self.plane_state(*current.fb.as_ref())];
            states.extend(self.cursor.as_ref().map(CursorPlane::state));
            self.surface
                .page_flip(states, true)
                .map_err(present_error)?;
            if let Some(cursor) = self.cursor.as_mut() {
                cursor.flushed();
            }
            self.flip_pending = true;
            return Ok(true);
        }

        #[expect(
            clippy::undocumented_unsafe_blocks,
            reason = "unjustified: one swap per lock rests on the draw closure telling the truth about drew"
        )]
        let bo = unsafe { self.gbm.lock_front_buffer() }.map_err(backend)?;
        let buffer = GbmBuffer::from_bo(bo, true);
        let fb = framebuffer_from_bo(&self.drm, &buffer, false).map_err(backend)?;

        let mut states = vec![self.plane_state(*fb.as_ref())];
        states.extend(self.cursor.as_ref().map(CursorPlane::state));
        let result = if self.modeset_done {
            self.surface.page_flip(states, true)
        } else {
            self.surface.commit(states, true)
        };
        result.map_err(present_error)?;
        self.modeset_done = true;
        self.flip_pending = true;
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.flushed();
        }

        self.previous = self.current.take();
        self.current = Some(Frame {
            fb,
            _buffer: buffer,
        });
        Ok(true)
    }
}
