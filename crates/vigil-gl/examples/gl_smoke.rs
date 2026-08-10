//! Smoke test for the GL path's foundation, on a render node — no DRM
//! master, no VT, no seat.
//!
//! What this proves:
//!   - an EGL context comes up on a GBM device;
//!   - Slint accepts `GlWindow` as a window adapter;
//!   - the real theme compiles and instantiates against the FemtoVG renderer.
//!
//! What it deliberately does NOT do is produce a picture. femtovg draws into
//! the default framebuffer, which exists only once there is an EGL window
//! surface over a GBM surface. Rendering without one silently yields black
//! pixels rather than an error, so a preview here would be a picture of
//! nothing dressed up as a passing test.
//!
//!   cargo run -p vigil-gl --example gl_smoke

use std::cell::RefCell;
use std::rc::Rc;

use slint::PhysicalSize;
use slint::platform::{Platform, WindowAdapter};
use vigil_gl::{GlContext, GlWindow};

const W: u32 = 1280;
const H: u32 = 800;

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
    let node = std::env::var("VIGIL_GL_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());

    let context = GlContext::open(std::path::Path::new(&node)).expect("gl context");
    println!("egl context on {node}: ok");

    let window = GlWindow::new(context, PhysicalSize::new(W, H)).expect("window");
    window.set_size(PhysicalSize::new(W, H));
    slint::platform::set_platform(Box::new(GlPlatform {
        window: RefCell::new(Some(window.clone())),
    }))
    .expect("set platform");
    println!("slint platform + femtovg window adapter: ok");

    let mut compiler = slint_interpreter::Compiler::default();
    compiler.set_style("fluent".into());
    let source = include_str!("../../../themes/default/theme.slint");
    let result = spin_on(compiler.build_from_source(source.into(), Default::default()));
    let mut failed = false;
    for diagnostic in result.diagnostics() {
        eprintln!("theme: {diagnostic}");
        failed = true;
    }
    let definition = result.component("DefaultTheme").expect("DefaultTheme");
    definition.create().expect("instantiate");
    assert!(!failed, "theme did not compile cleanly for the GL renderer");
    println!("default theme compiles and instantiates under femtovg: ok");
    println!("\nscanout is not wired up yet: see the crate docs (issue #17)");
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
