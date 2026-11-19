mod cursor;

pub use cursor::CursorPlane;

use smithay::backend::drm::DrmError;
use smithay::reexports::rustix::io::Errno;
use smithay::utils::Transform;
use vigil_core::PresentError;

pub fn backend(e: impl std::fmt::Display) -> PresentError {
    PresentError::Backend(e.to_string())
}

pub fn present_error(e: DrmError) -> PresentError {
    match e {
        DrmError::DeviceInactive => PresentError::DeviceLost,
        DrmError::Access(ref a)
            if matches!(
                Errno::from_io_error(&a.source),
                Some(Errno::NODEV | Errno::ACCESS | Errno::PERM)
            ) =>
        {
            PresentError::DeviceLost
        }
        e => backend(e),
    }
}

pub fn plane_transform(transform: u8) -> Transform {
    match transform % 4 {
        1 => Transform::_90,
        2 => Transform::_180,
        3 => Transform::_270,
        _ => Transform::Normal,
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
}
