//! The full GL path: render the real theme with femtovg into a GBM surface
//! and scan the result out on a real CRTC.
//!
//! Needs DRM master, so run it through the GPU harness:
//!
//!   cargo build -p vigil-gl --example gl_modeset
//!   tests/gpu/run.sh -- target/debug/examples/gl_modeset
//!   tests/gpu/run.sh --accel -- target/debug/examples/gl_modeset
//!
//! With `--screenshot`, the harness captures the guest's display afterwards,
//! which is the only way to prove the frame really reached the screen rather
//! than merely being committed without error.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use slint::ComponentHandle;
use slint::PhysicalSize;
use slint::platform::{Platform, WindowAdapter};
use smithay::backend::allocator::gbm::GbmBuffer;
use smithay::backend::drm::{DrmDeviceFd, gbm::framebuffer_from_bo};
use smithay::utils::DeviceFd;
use vigil_gl::{GlContext, GlSurface, GlWindow};

use drm::control::Device as ControlDevice;

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

/// A card fd usable as a DRM control device.
struct Card(DrmDeviceFd);

impl std::os::fd::AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

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
    let drm = Card(DrmDeviceFd::new(DeviceFd::from(
        fd.try_clone().expect("dup card fd"),
    )));

    // Pick the first connected connector and its preferred mode.
    let resources = drm.resource_handles().expect("drm resources");
    let (connector, mode) = resources
        .connectors()
        .iter()
        .filter_map(|handle| drm.get_connector(*handle, false).ok())
        .find(|c| c.state() == drm::control::connector::State::Connected && !c.modes().is_empty())
        .map(|c| {
            let mode = *c
                .modes()
                .iter()
                .find(|m| {
                    m.mode_type()
                        .contains(drm::control::ModeTypeFlags::PREFERRED)
                })
                .unwrap_or(&c.modes()[0]);
            (c, mode)
        })
        .expect("no connected connector with a mode");
    let (w, h) = mode.size();
    let (w, h) = (w as u32, h as u32);
    println!(
        "connector {:?}: {w}x{h}@{}",
        connector.interface(),
        mode.vrefresh()
    );

    let crtc = *resources.crtcs().first().expect("no crtc");

    let context = Rc::new(GlContext::from_fd(fd).expect("gl context"));
    let surface = GlSurface::new(context, w, h).expect("gbm window surface");
    let gbm = surface.window();

    let window = GlWindow::with_surface(surface, PhysicalSize::new(w, h)).expect("window");
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
            "clock-text",
            slint_interpreter::Value::String("13:37".into()),
        )
        .expect("clock");
    instance
        .set_property(
            "prompt-text",
            slint_interpreter::Value::String("Password:".into()),
        )
        .expect("prompt");
    instance.show().expect("show");

    window.render().expect("render");
    println!("rendered the default theme through femtovg: ok");

    // SAFETY: a swap just happened; the buffer is released after the flip.
    let bo = unsafe { gbm.lock_front_buffer() }.expect("lock front buffer");
    let buffer = GbmBuffer::from_bo(bo, true);
    let fb = framebuffer_from_bo(&drm.0, &buffer, false).expect("drm framebuffer");
    println!("drm framebuffer from the gbm buffer: ok");

    drm.set_crtc(
        crtc,
        Some(*fb.as_ref()),
        (0, 0),
        &[connector.handle()],
        Some(mode),
    )
    .expect("set_crtc");
    println!("MODESET OK: a GL frame is on the CRTC");

    // Hold the mode long enough for the harness to capture the display.
    let hold = std::env::var("VIGIL_GL_HOLD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
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
