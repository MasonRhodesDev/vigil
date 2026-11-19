#![deny(unsafe_code)]
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

struct Card(DrmDeviceFd);

impl std::os::fd::AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

#[allow(unsafe_code)]
fn main() {
    let node = std::env::var("VIGIL_GL_NODE").unwrap_or_else(|_| "/dev/dri/card0".into());

    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(&node)
        .expect("open card");
    let fd = Arc::new(OwnedFd::from(file));
    let drm = Card(DrmDeviceFd::new(DeviceFd::from(
        fd.try_clone().expect("dup card fd"),
    )));

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
    vigil_log::out!(
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
        vigil_log::say!("theme: {diagnostic}");
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
    vigil_log::out!("rendered the default theme through femtovg: ok");

    // SAFETY: window.render() above swapped this surface exactly once.
    let bo = unsafe { gbm.lock_front_buffer() }.expect("lock front buffer");
    let buffer = GbmBuffer::from_bo(bo, true);
    let fb = framebuffer_from_bo(&drm.0, &buffer, false).expect("drm framebuffer");
    vigil_log::out!("drm framebuffer from the gbm buffer: ok");

    drm.set_crtc(
        crtc,
        Some(*fb.as_ref()),
        (0, 0),
        &[connector.handle()],
        Some(mode),
    )
    .expect("set_crtc");
    vigil_log::out!("MODESET OK: a GL frame is on the CRTC");

    let hold = std::env::var("VIGIL_GL_HOLD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    std::thread::sleep(std::time::Duration::from_millis(hold));
    vigil_log::out!("DONE");
}

#[allow(unsafe_code)]
fn spin_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    // SAFETY: every vtable entry ignores the null data pointer, and clone hands
    // back the same no-op waker.
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
}
