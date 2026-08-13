//! The point of the abstraction, demonstrated: the *same* `OutputWindow`
//! the greeter uses, driving GL instead of software.
//!
//! Nothing here touches a scene API that the GL path implements separately.
//! `set_clock`, `show_prompt`, `set_users` and the rest are the greeter's own
//! code; only the backend differs.
//!
//!   cargo build -p vigil-present-gl --example gl_shared_scene
//!   tests/gpu/run.sh --screenshot /tmp/shared.ppm -- target/debug/examples/gl_shared_scene

use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use smithay::backend::drm::{DrmDevice, DrmDeviceFd};
use smithay::reexports::drm::control::Device as ControlDevice;
use smithay::utils::DeviceFd;
use vigil_core::{AuthUi, Canvas, OutputId, Presenter};
use vigil_gl::{GlBackend, GlContext, GlWindow};
use vigil_present_gl::GbmPresenter;
use vigil_theme::Theme;
use vigil_ui::{OutputWindow, VigilPlatform};

const FRAMES: usize = 30;

fn main() {
    let node = std::env::var("VIGIL_GL_NODE").unwrap_or_else(|_| "/dev/dri/card0".into());

    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&node)
        .expect("open card");
    let fd = Arc::new(OwnedFd::from(file));
    let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd.try_clone().expect("dup card fd")));
    let (mut device, _notifier) = DrmDevice::new(drm_fd.clone(), false).expect("drm device");

    let resources = device.resource_handles().expect("drm resources");
    let connector = resources
        .connectors()
        .iter()
        .filter_map(|handle| device.get_connector(*handle, false).ok())
        .find(|c| {
            c.state() == smithay::reexports::drm::control::connector::State::Connected
                && !c.modes().is_empty()
        })
        .expect("no connected connector");
    let mode = connector.modes()[0];
    let crtc = *resources.crtcs().first().expect("no crtc");
    let (w, h) = mode.size();
    let (w, h) = (w as u32, h as u32);

    let surface = device
        .create_surface(crtc, mode, &[connector.handle()])
        .expect("drm surface");
    let context = Rc::new(GlContext::from_fd(fd).expect("gl context"));
    let (mut presenter, gl) = GbmPresenter::new(surface, drm_fd, context, 1.0, 0).expect("presenter");

    // The platform is installed first, then told to vend this GL window for
    // the next instantiation: a component is bound to whatever adapter
    // existed when it was created, and the GL window only exists now.
    let platform = VigilPlatform::install().expect("platform");
    let gl_window = GlWindow::with_surface(gl, slint::PhysicalSize::new(w, h)).expect("window");
    gl_window.set_size(slint::PhysicalSize::new(w, h));
    platform.use_next_adapter(gl_window.clone());

    let theme = Theme::load_or_default(None);
    let component = theme.instantiate().expect("instantiate");
    platform.clear_adapter_override();
    // If a software adapter was created for this component, the override did
    // not cover it and femtovg is rendering a scene nothing is attached to --
    // which succeeds, and shows black.
    assert!(
        platform.claim_last_adapter().is_none(),
        "theme bound to a software adapter, not the GL one"
    );
    let mut window = OutputWindow::with_backend(
        OutputId(1),
        w,
        h,
        1.0,
        component,
        Box::new(GlBackend::new(gl_window)),
    )
    .expect("output window");

    // Ordinary greeter scene calls -- not a GL-specific API in sight.
    window.set_panel_visible(true);
    window.set_users(&["mason".into(), "Other…".into()]);
    window.set_user_index(0);
    window.set_sessions(&["Hyprland".into(), "Sway".into()]);
    window.set_session_index(0);
    window.show_prompt("Password:", true);
    window.set_status_banner("rendered by GL through the shared scene");

    let mut presented = 0usize;
    for frame in 0..FRAMES {
        window.set_clock(&format!("13:{frame:02}"));
        vigil_ui::advance_timers();

        let drew = presenter
            .with_frame(&mut |canvas| {
                assert!(matches!(canvas, Canvas::Gl { .. }), "expected a GL canvas");
                window.render(canvas)
            })
            .unwrap_or_else(|e| panic!("frame {frame}: {e}"));
        if drew {
            presented += 1;
            wait_for_flip(&device, frame);
        }
    }

    assert!(presented > 0, "nothing was ever presented");
    println!("{presented}/{FRAMES} frames presented through the shared scene");

    // An unchanged scene must stop presenting, or an idle login screen flips
    // forever. This is the GL equivalent of the software partial-repaint
    // bookkeeping, and the reason GlBackend keeps the last view.
    let idle = presenter
        .with_frame(&mut |canvas| window.render(canvas))
        .expect("idle frame");
    assert!(!idle, "an unchanged scene must not keep presenting");
    println!("idle scene does not present: ok");

    println!("MODESET OK: a GL frame is on the CRTC");
    let hold = std::env::var("VIGIL_GL_HOLD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    std::thread::sleep(std::time::Duration::from_millis(hold));
    println!("SHARED SCENE OK");
}

/// A flip submitted while the previous is pending is refused with EBUSY.
fn wait_for_flip(device: &DrmDevice, frame: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match device.receive_events() {
            Ok(events) => {
                for event in events {
                    if matches!(event, smithay::reexports::drm::control::Event::PageFlip(_)) {
                        return;
                    }
                }
            }
            Err(e) => panic!("frame {frame}: drm events: {e}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "frame {frame}: no page-flip event within 2s"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
