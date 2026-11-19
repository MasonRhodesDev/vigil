#![deny(unsafe_code)]

use std::ptr::NonNull;

use smithay::backend::drm::{DrmSurface, PlaneConfig, PlaneDamageClips, PlaneState};
use smithay::reexports::drm::buffer::{Buffer as _, DrmFourcc};
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::{Device as ControlDevice, framebuffer};
use smithay::reexports::rustix;
use smithay::utils::{Rectangle, Transform};
use vigil_core::{BufferDamage, Canvas, Damage, FrameTarget, PresentError, Presenter};
use vigil_present_kms::{CursorPlane, backend, present_error};

const BPP: u32 = 32;
const DEPTH: u32 = 24;

struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapped bytes are owned and reached only through `&mut self`, as with a `Box<[u8]>`.
#[allow(unsafe_code)]
unsafe impl Send for Mapping {}
// SAFETY: `&Mapping` exposes no access to the bytes, so sharing it shares nothing.
#[allow(unsafe_code)]
unsafe impl Sync for Mapping {}

impl Mapping {
    fn new(surface: &DrmSurface, buffer: &DumbBuffer) -> std::io::Result<Self> {
        let mut handle = *buffer;
        let mut mapped = surface.map_dumb_buffer(&mut handle)?;
        let bytes: &mut [u8] = mapped.as_mut();
        let mapping = Self {
            ptr: NonNull::from(&mut *bytes).cast(),
            len: bytes.len(),
        };
        #[expect(
            clippy::mem_forget,
            reason = "drm-rs unmaps when its guard drops; this type owns the unmap instead"
        )]
        std::mem::forget(mapped);
        Ok(mapping)
    }

    #[allow(unsafe_code)]
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: ptr and len describe a live read-write mapping this value
        // owns until Drop, and `&mut self` makes this the only view of it.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

#[allow(unsafe_code)]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr and len are exactly the mapping drm-rs created, no slice
        // from `bytes` outlives `&mut self`, and Drop runs once.
        let _ = unsafe { rustix::mm::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

struct Slot {
    buffer: DumbBuffer,
    fb: framebuffer::Handle,
    mapping: Mapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flip {
    Modeset,
    Page,
}

#[derive(Default)]
struct Pacing {
    back: usize,
    modeset_done: bool,
    staged: bool,
    flip_pending: bool,
}

impl Pacing {
    fn shown(&self) -> usize {
        self.back ^ 1
    }

    fn frame_flip(&self) -> Option<Flip> {
        (self.staged && !self.flip_pending).then_some(if self.modeset_done {
            Flip::Page
        } else {
            Flip::Modeset
        })
    }

    fn cursor_flip(&self, cursor_dirty: bool) -> bool {
        cursor_dirty && self.modeset_done && !self.flip_pending
    }

    fn frame_flipped(&mut self) {
        self.staged = false;
        self.modeset_done = true;
        self.flip_pending = true;
        self.back ^= 1;
    }

    fn cursor_flipped(&mut self) {
        self.flip_pending = true;
    }

    fn vblank(&mut self) {
        self.flip_pending = false;
    }

    fn invalidate(&mut self) {
        self.modeset_done = false;
        self.flip_pending = false;
    }
}

pub struct DumbBufferPresenter {
    surface: DrmSurface,
    slots: [Slot; 2],
    damage: BufferDamage,
    cursor: Option<CursorPlane>,
    pacing: Pacing,
    width: u32,
    height: u32,
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
            let mapping = Mapping::new(&surface, &buffer).map_err(backend)?;
            Ok(Slot {
                buffer,
                fb,
                mapping,
            })
        };
        let slots = [mk_slot()?, mk_slot()?];

        Ok(Self {
            surface,
            damage: BufferDamage::new(slots.len()),
            slots,
            cursor: None,
            pacing: Pacing::default(),
            width,
            height,
        })
    }

    pub fn claim_cursor(&mut self, scale: f32, transform: u8) -> Result<(), String> {
        let scene = if transform % 2 == 1 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let cursor = CursorPlane::claim(&self.surface, scale, transform, scene)?;
        let shown = [
            self.plane_state(self.slots[0].fb, None),
            cursor.state_at(Some((0, 0))),
        ];
        self.surface
            .test_state(shown, !self.pacing.modeset_done)
            .map_err(|e| format!("the driver refused the cursor plane: {e}"))?;
        self.cursor = Some(cursor);
        Ok(())
    }

    fn damage_clips(&self) -> Option<PlaneDamageClips> {
        let shown = self.damage.behind(self.pacing.shown());
        if *shown == Damage::Full {
            return None;
        }
        let rects = shown
            .rects(self.width, self.height)
            .into_iter()
            .map(|rect| {
                Rectangle::new(
                    (rect.x as i32, rect.y as i32).into(),
                    (rect.w as i32, rect.h as i32).into(),
                )
            });
        PlaneDamageClips::from_damage(
            self.surface.device_fd(),
            Rectangle::from_size((self.width as f64, self.height as f64).into()),
            Rectangle::from_size((self.width as i32, self.height as i32).into()),
            rects,
        )
        .ok()
        .flatten()
    }

    fn plane_state<'a>(
        &self,
        fb: framebuffer::Handle,
        clips: Option<&'a PlaneDamageClips>,
    ) -> PlaneState<'a> {
        PlaneState {
            handle: self.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((self.width as f64, self.height as f64).into()),
                dst: Rectangle::from_size((self.width as i32, self.height as i32).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: clips.map(PlaneDamageClips::blob),
                fb,
                fence: None,
            }),
        }
    }

    fn submit(
        &mut self,
        flip: Flip,
        fb: framebuffer::Handle,
        clips: Option<&PlaneDamageClips>,
    ) -> Result<(), PresentError> {
        let send = |surface: &DrmSurface, states: Vec<PlaneState<'_>>| match flip {
            Flip::Page => surface.page_flip(states, true),
            Flip::Modeset => surface.commit(states, true),
        };
        let mut states = vec![self.plane_state(fb, clips)];
        states.extend(self.cursor.as_ref().map(CursorPlane::state));
        let Err(error) = send(&self.surface, states) else {
            if let Some(cursor) = self.cursor.as_mut() {
                cursor.flushed();
            }
            return Ok(());
        };
        let Some(cursor) = self.cursor.take() else {
            return Err(present_error(error));
        };
        let without = vec![self.plane_state(fb, clips), cursor.state_at(None)];
        if send(&self.surface, without).is_err() {
            self.cursor = Some(cursor);
            return Err(present_error(error));
        }
        eprintln!(
            "vigil: crtc {:?}: giving up the cursor plane, a flip with it failed: {error}",
            self.surface.crtc()
        );
        Ok(())
    }
}

impl Presenter for DumbBufferPresenter {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn invalidate(&mut self) {
        self.pacing.invalidate();
        self.damage.invalidate();
        if let Some(cursor) = self.cursor.as_mut() {
            cursor.invalidate();
        }
    }

    fn vblank(&mut self) {
        self.pacing.vblank();
    }

    fn flip_pending(&self) -> bool {
        self.pacing.flip_pending
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

    fn flush_cursor(&mut self) -> Result<bool, PresentError> {
        let dirty = self.cursor.as_ref().is_some_and(CursorPlane::is_dirty);
        if !self.pacing.cursor_flip(dirty) {
            return Ok(false);
        }
        let shown = self.slots[self.pacing.shown()].fb;
        self.submit(Flip::Page, shown, None)?;
        self.pacing.cursor_flipped();
        Ok(true)
    }

    fn with_frame(
        &mut self,
        draw: &mut dyn FnMut(Canvas<'_>) -> bool,
    ) -> Result<bool, PresentError> {
        if self.pacing.flip_pending {
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
        let back = self.pacing.back;
        let slot = &mut self.slots[back];
        let stride = slot.buffer.pitch() as usize;
        let mut frame = self.damage.begin(back);
        let drew = draw(Canvas::Cpu(FrameTarget {
            buffer: slot.mapping.bytes(),
            width: self.width,
            height: self.height,
            stride,
            damage: Some(&mut frame),
        }));
        self.damage.finish(back, frame, drew);
        self.pacing.staged = drew;
        Ok(drew)
    }

    fn commit_staged(&mut self) -> Result<bool, PresentError> {
        let Some(flip) = self.pacing.frame_flip() else {
            return Ok(false);
        };
        let fb = self.slots[self.pacing.back].fb;
        let clips = (flip == Flip::Page).then(|| self.damage_clips()).flatten();
        self.submit(flip, fb, clips.as_ref())?;
        self.pacing.frame_flipped();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> Pacing {
        Pacing {
            staged: true,
            ..Pacing::default()
        }
    }

    #[test]
    fn the_first_frame_is_a_modeset_and_later_frames_are_page_flips() {
        let mut pacing = staged();
        assert_eq!(pacing.frame_flip(), Some(Flip::Modeset));
        pacing.frame_flipped();
        pacing.vblank();
        pacing.staged = true;
        assert_eq!(pacing.frame_flip(), Some(Flip::Page));
    }

    #[test]
    fn nothing_is_submitted_until_the_pending_flip_completes() {
        let mut pacing = staged();
        pacing.frame_flipped();
        pacing.staged = true;
        assert_eq!(pacing.frame_flip(), None);
        assert!(!pacing.cursor_flip(true));
        pacing.vblank();
        assert_eq!(pacing.frame_flip(), Some(Flip::Page));
        assert!(pacing.cursor_flip(true));
        pacing.cursor_flipped();
        assert_eq!(pacing.frame_flip(), None);
        assert!(!pacing.cursor_flip(true));
    }

    #[test]
    fn a_cursor_flip_needs_a_frame_on_screen_and_reuses_it() {
        let mut pacing = staged();
        assert!(!pacing.cursor_flip(true), "nothing is on screen yet");
        let first = pacing.back;
        pacing.frame_flipped();
        pacing.vblank();
        assert!(pacing.cursor_flip(true));
        assert!(!pacing.cursor_flip(false), "a clean cursor needs no flip");
        assert_eq!(pacing.shown(), first);
        pacing.cursor_flipped();
        assert_eq!(pacing.shown(), first, "a cursor flip swaps no buffers");
        assert_ne!(pacing.back, first);
    }

    #[test]
    fn invalidate_clears_the_gate_and_asks_for_a_modeset() {
        let mut pacing = staged();
        pacing.frame_flipped();
        pacing.invalidate();
        assert!(!pacing.cursor_flip(true));
        pacing.staged = true;
        assert_eq!(pacing.frame_flip(), Some(Flip::Modeset));
    }
}
