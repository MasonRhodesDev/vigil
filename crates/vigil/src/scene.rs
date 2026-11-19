#[cfg(test)]
use std::path::Path;

pub use vigil_scene::{Scene, ease, parse_color};

#[allow(unsafe_code)]
pub fn monotonic_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a live local timespec and CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_follows_the_short_side() {
        let s = Scene::default();
        let g = s.geometry(3440, 1440);
        assert_eq!((g.logo, g.glow, g.logo_x, g.logo_y), (230, 460, 1605, 562));
        let g = s.geometry(2160, 3840);
        assert_eq!((g.logo, g.glow, g.logo_x, g.logo_y), (346, 692, 907, 1632));
        let g = s.geometry(1920, 1080);
        assert_eq!((g.logo, g.glow, g.logo_x, g.logo_y), (173, 346, 874, 421));
    }

    #[test]
    fn fixed_reference_gives_one_size_and_a_fixed_offset() {
        let s = Scene::parse(r#"{"version":2,"ref_short_side":1440}"#).unwrap();
        assert_eq!(s.ref_short_side, Some(1440));
        let wide = s.geometry(3440, 1440);
        let hd = s.geometry(1920, 1080);
        let tall = s.geometry(2160, 3840);
        assert_eq!((wide.logo, hd.logo, tall.logo), (230, 230, 230));
        assert_eq!(wide.logo_y, 562);
        assert_eq!(hd.logo_y, 540 - 43 - 115);
        assert_eq!(tall.logo_y, 1920 - 43 - 115);
        assert_eq!(s.tag_geometry(1920, 1080), s.tag_geometry(3440, 1440));
        for text in [
            r#"{"version":2,"ref_short_side":null}"#,
            r#"{"version":2,"ref_short_side":0}"#,
        ] {
            assert_eq!(Scene::parse(text).unwrap().ref_short_side, None);
        }
    }

    #[test]
    fn json_export_round_trips_to_the_defaults() {
        let text = include_str!("../../../tests/fixtures/scene.json");
        assert_eq!(Scene::parse(text).unwrap(), Scene::default());
    }

    #[test]
    fn other_versions_and_junk_are_refused() {
        assert!(Scene::parse("{\"version\": 3}").is_err());
        assert!(Scene::parse("nope").is_err());
        assert_eq!(
            Scene::load(Path::new("/nonexistent/scene.json")),
            Scene::default()
        );
    }

    #[test]
    fn partial_spec_keeps_the_rest_of_the_defaults() {
        let s =
            Scene::parse(r##"{"version":2,"palette":{"bg":"#102030","accent":"nope"}}"##).unwrap();
        assert_eq!(s.bg, [0x10, 0x20, 0x30]);
        assert_eq!(s.accent, Scene::default().accent);
    }

    #[test]
    fn motion_eases_in_and_out_and_breathes() {
        let s = Scene::default();
        assert_eq!(s.motion_level(0.0, None), 0.0);
        assert_eq!(s.motion_level(s.in_s, None), 1.0);
        assert!((s.motion_level(s.in_s / 2.0, None) - 0.5).abs() < 1e-9);
        assert_eq!(s.motion_level(5.0 + s.out_s, Some(5.0)), 0.0);
        assert!(s.motion_level(5.0 + s.out_s / 2.0, Some(5.0)) < 1.0);
        assert!((s.breath(0.0)).abs() < 1e-9);
        assert!((s.breath(s.breath_period_s / 2.0) - 1.0).abs() < 1e-9);
        assert!((s.glow_level(1.0, None, s.breath_period_s / 2.0) - s.glow_max).abs() < 1e-9);
        assert!((s.glow_level(1.0, None, 0.0) - s.glow_min).abs() < 1e-9);
    }

    #[test]
    fn tag_sizes_have_floors() {
        let t = Scene::default().tag_geometry(300, 200);
        assert_eq!((t.border, t.pt), (4, 10));
    }
}
