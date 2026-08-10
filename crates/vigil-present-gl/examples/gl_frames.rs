//! Present many GL frames through `GbmPresenter` on a real CRTC.
//!
//! The point is the count. A GBM surface owns a small fixed pool of buffers,
//! and a presenter that never releases them runs it dry after two or three
//! frames -- a bug a one-frame demo cannot see. This drives enough frames
//! that a leak has to surface.
//!
//!   cargo build -p vigil-present-gl --example gl_frames
//!   tests/gpu/run.sh --screenshot /tmp/frames.ppm -- target/debug/examples/gl_frames
//!   tests/gpu/run.sh --accel -- target/debug/examples/gl_frames

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use slint::ComponentHandle;
use slint::PhysicalSize;
use slint::platform::{Platform, WindowAdapter};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd};
use smithay::utils::DeviceFd;
use vigil_core::{Canvas, Presenter};
use vigil_gl::{GlContext, GlWindow};
use vigil_present_gl::GbmPresenter;

use smithay::reexports::drm::control::Device as ControlDevice;

const FRAMES: usize = 60;

struct GlPlatform {
    window: RefCell<Option<Rc<GlWindow>>>,
}

impl Platform for GlPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        self.window
            .borrow()
            .clone()
            .map(|w| w as Rc<dyn WindowAdapter>)
            .ok_or_else(|| slint::PlatformError::Other("no window".into()))
    }
}

fn main() {
    let node = std::env::var("VIGIL_GL_NODE").unwrap_or_else(|_| "/dev/dri/card0".into());

    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&node)
        .expect("open card");
    let fd = Arc::new(OwnedFd::from(file));
    // Duplicate rather than re-open: DRM master rides on the open file
    // description, so a second open would not be master.
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
    let mode = *connector
        .modes()
        .iter()
        .find(|m| {
            m.mode_type()
                .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
        })
        .unwrap_or(&connector.modes()[0]);
    let crtc = *resources.crtcs().first().expect("no crtc");
    let (w, h) = mode.size();
    let (w, h) = (w as u32, h as u32);
    println!("connector {:?}: {w}x{h}", connector.interface());

    let surface = device
        .create_surface(crtc, mode, &[connector.handle()])
        .expect("drm surface");

    let context = Rc::new(GlContext::from_fd(fd).expect("gl context"));
    let (mut presenter, gl) = GbmPresenter::new(surface, drm_fd, context).expect("presenter");

    let window = GlWindow::with_surface(gl, PhysicalSize::new(w, h)).expect("window");
    window.set_size(PhysicalSize::new(w, h));
    slint::platform::set_platform(Box::new(GlPlatform {
        window: RefCell::new(Some(window.clone())),
    }))
    .expect("set platform");

    let mut compiler = slint_interpreter::Compiler::default();
    compiler.set_style("fluent".into());
    let source = include_str!("../../../themes/default/theme.slint");
    let result = spin_on(compiler.build_from_source(source.into(), Default::default()));
    for diagnostic in result.diagnostics() {
        eprintln!("theme: {diagnostic}");
    }
    let definition = result.component("DefaultTheme").expect("DefaultTheme");
    let instance = definition.create().expect("instantiate");
    instance
        .set_property("panel-visible", slint_interpreter::Value::Bool(true))
        .expect("panel");
    instance
        .set_property(
            "prompt-text",
            slint_interpreter::Value::String("Password:".into()),
        )
        .expect("prompt");
    instance.show().expect("show");

    for frame in 0..FRAMES {
        // A changing clock keeps the scene genuinely dirty, so every
        // iteration really renders and swaps rather than short-circuiting.
        instance
            .set_property(
                "clock-text",
                slint_interpreter::Value::String(format!("13:{frame:02}").into()),
            )
            .expect("clock");
        slint::platform::update_timers_and_animations();

        let presented = presenter
            .with_frame(&mut |canvas| {
                let Canvas::Gl { width, height } = canvas else {
                    panic!("GbmPresenter handed a CPU canvas");
                };
                assert_eq!((width, height), (w, h), "canvas size disagrees with mode");
                window.render().expect("render");
                true
            })
            .unwrap_or_else(|e| panic!("frame {frame}: {e}"));
        assert!(presented, "frame {frame} did not present");

        // Wait for the flip to complete before submitting the next one.
        // Without this the presenter is refused with EBUSY within a handful
        // of frames -- which is exactly what this example caught.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        'wait: loop {
            match device.receive_events() {
                Ok(events) => {
                    for event in events {
                        if matches!(event, smithay::reexports::drm::control::Event::PageFlip(_)) {
                            break 'wait;
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

    println!("{FRAMES} FRAMES OK (buffer pool held up)");

    // Hold the last frame long enough for a screenshot.
    let hold = std::env::var("VIGIL_GL_HOLD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    println!("MODESET OK: a GL frame is on the CRTC");
    std::thread::sleep(std::time::Duration::from_millis(hold));
    println!("DONE");
}

/// The interpreter's compile future resolves immediately without a runtime.
fn spin_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: the vtable is a no-op waker over a null pointer it never reads.
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}
