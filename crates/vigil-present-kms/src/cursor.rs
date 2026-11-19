use smithay::backend::drm::{DrmSurface, PlaneClaim, PlaneConfig, PlaneInfo, PlaneState};
use smithay::reexports::drm::buffer::{Buffer as _, DrmFourcc};
use smithay::reexports::drm::control::{
    Device as ControlDevice, dumbbuffer::DumbBuffer, framebuffer, plane,
};
use smithay::utils::{Rectangle, Transform};
use vigil_core::scene_to_panel;

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

pub struct CursorPlane {
    plane: plane::Handle,
    _claim: PlaneClaim,
    _buffer: DumbBuffer,
    fb: framebuffer::Handle,
    size: (u32, u32),
    transform: u8,
    scene: (u32, u32),
    pos: Option<(i32, i32)>,
    dirty: bool,
}

fn find(surface: &DrmSurface) -> Option<&PlaneInfo> {
    surface
        .planes()
        .cursor
        .iter()
        .find(|p| p.formats.iter().any(|f| f.code == DrmFourcc::Argb8888))
}

fn plane_size(info: &PlaneInfo) -> (u32, u32) {
    info.size_hints
        .as_deref()
        .and_then(<[_]>::first)
        .map_or((64, 64), |s| (u32::from(s.w), u32::from(s.h)))
}

fn usable(surface: &DrmSurface, transform: u8) -> Result<(&PlaneInfo, (u32, u32)), String> {
    if surface.is_legacy() {
        return Err("legacy modesetting has no cursor plane".into());
    }
    let info = find(surface).ok_or("no ARGB8888 cursor plane")?;
    let (pw, ph) = plane_size(info);
    if transform % 2 == 1 && pw != ph {
        return Err(format!(
            "the {pw}x{ph} cursor plane cannot hold a quarter-turned cursor"
        ));
    }
    Ok((info, (pw, ph)))
}

impl CursorPlane {
    pub fn probe(surface: &DrmSurface, transform: u8) -> Result<(), String> {
        usable(surface, transform % 4).map(|_| ())
    }

    pub fn claim(
        surface: &DrmSurface,
        scale: f32,
        transform: u8,
        scene: (u32, u32),
    ) -> Result<Self, String> {
        let transform = transform % 4;
        let (info, (pw, ph)) = usable(surface, transform)?;
        let max_scale = (pw as f32 / vigil_core::CURSOR[0].len() as f32)
            .min(ph as f32 / vigil_core::CURSOR.len() as f32);
        let (argb, cw, ch) = vigil_core::cursor_argb(scale.min(max_scale));
        let claim = surface
            .claim_plane(info.handle)
            .ok_or("the cursor plane is claimed by another output")?;
        let mut buffer = surface
            .create_dumb_buffer((pw, ph), DrmFourcc::Argb8888, 32)
            .map_err(|e| format!("cursor buffer: {e}"))?;
        {
            let pitch = buffer.pitch() as usize;
            let mut mapping = surface
                .map_dumb_buffer(&mut buffer)
                .map_err(|e| format!("cursor buffer map: {e}"))?;
            let bytes: &mut [u8] = mapping.as_mut();
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
        let fb = surface
            .add_framebuffer(&buffer, 32, 32)
            .map_err(|e| format!("cursor framebuffer: {e}"))?;
        Ok(Self {
            plane: info.handle,
            _claim: claim,
            _buffer: buffer,
            fb,
            size: (pw, ph),
            transform,
            scene,
            pos: None,
            dirty: false,
        })
    }

    pub fn set(&mut self, pos: Option<(i32, i32)>) {
        if self.pos != pos {
            self.pos = pos;
            self.dirty = true;
        }
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn flushed(&mut self) {
        self.dirty = false;
    }

    pub fn state(&self) -> PlaneState<'_> {
        self.state_at(self.pos)
    }

    pub fn state_at(&self, pos: Option<(i32, i32)>) -> PlaneState<'_> {
        PlaneState {
            handle: self.plane,
            config: pos.map(|pos| PlaneConfig {
                src: Rectangle::from_size((f64::from(self.size.0), f64::from(self.size.1)).into()),
                dst: Rectangle::new(
                    cursor_dst(self.transform, self.scene, self.size, pos).into(),
                    (self.size.0 as i32, self.size.1 as i32).into(),
                ),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: self.fb,
                fence: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
