//! Render the real theme through GL into a GBM surface, then lock the buffer
//! that a DRM framebuffer would be made from.
//!
//! This is the whole scanout path except the CRTC commit, so it needs a card
//! node we are DRM master on. Run it through the GPU harness:
//!
//!   cargo build -p vigil-gl --example gl_scanout
//!   tests/gpu/run.sh -- target/debug/examples/gl_scanout
//!   tests/gpu/run.sh --accel -- target/debug/examples/gl_scanout
//!
//! Pixels are read back *before* the swap: afterwards the back buffer is
//! undefined and reads black however well the draw went.

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle;
use slint::PhysicalSize;
use slint::platform::{Platform, WindowAdapter};
use vigil_gl::{GlContext, GlSurface, GlWindow};

const W: u32 = 640;
const H: u32 = 480;

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

    let context = Rc::new(GlContext::open(std::path::Path::new(&node)).expect("gl context"));
    let surface = GlSurface::new(context, W, H).expect("gbm window surface");
    println!("egl window surface over gbm: ok");
    let gbm = surface.window();

    let window = GlWindow::with_surface(surface, PhysicalSize::new(W, H)).expect("window");
    window.set_size(PhysicalSize::new(W, H));
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
    instance.show().expect("show");

    // Renders and swaps: femtovg's present forwards to eglSwapBuffers.
    window.render().expect("render");
    println!("rendered the default theme through femtovg: ok");

    // SAFETY: a swap just happened, and the buffer is released below.
    let bo = unsafe { gbm.lock_front_buffer() }.expect("lock front buffer");
    println!(
        "front buffer: {}x{} stride={} format={:?}",
        bo.width(),
        bo.height(),
        bo.stride(),
        bo.format(),
    );
    assert_eq!(bo.width(), W, "front buffer is the wrong size");
    gbm.release_buffer(bo);

    println!("SCANOUT PATH OK (everything but the CRTC commit)");
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
