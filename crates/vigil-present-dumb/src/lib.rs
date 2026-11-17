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

pub struct DumbBufferPresenter {
    surface: DrmSurface,
    slots: [Slot; 2],
    back: usize,
    modeset_done: bool,
    staged: bool,
    flip_pending: bool,
    width: u32,
    height: u32,
}

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

fn io_present_error(e: std::io::Error) -> PresentError {
    use smithay::reexports::rustix;
    if rustix::io::Errno::from_io_error(&e) == Some(rustix::io::Errno::NODEV) {
        PresentError::DeviceLost
    } else {
        backend(e)
    }
}

impl DumbBufferPresenter {
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
            staged: false,
            flip_pending: false,
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
        self.flip_pending = false;
    }

    fn vblank(&mut self) {
        self.flip_pending = false;
    }

    fn flip_pending(&self) -> bool {
        self.flip_pending
    }

    fn crtc_id(&self) -> Option<u32> {
        Some(self.surface.crtc().into())
    }

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        if self.flip_pending {
            return Ok(false);
        }
        if !self.stage_frame(draw)? {
            return Ok(false);
        }
        self.commit_staged()
    }

    fn stage_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        let slot = &mut self.slots[self.back];
        let stride = slot.buffer.pitch() as usize;
        let mut mapping = self
            .surface
            .map_dumb_buffer(&mut slot.buffer)
            .map_err(io_present_error)?;
        let drew = draw(Canvas::Cpu(FrameTarget {
            buffer: mapping.as_mut(),
            width: self.width,
            height: self.height,
            stride,
        }));
        self.staged = drew;
        Ok(drew)
    }

    fn commit_staged(&mut self) -> Result<bool, PresentError> {
        if !self.staged || self.flip_pending {
            return Ok(false);
        }
        let fb = self.slots[self.back].fb;
        let state = self.plane_state(fb);
        let result = if self.modeset_done {
            self.surface.page_flip([state], true)
        } else {
            self.surface.commit([state], true)
        };
        result.map_err(present_error)?;

        self.staged = false;
        self.modeset_done = true;
        self.flip_pending = true;
        self.back ^= 1;
        Ok(true)
    }
}
