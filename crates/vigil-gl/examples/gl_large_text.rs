//! Large text through the GL path draws glyphs, not boxes.
//!
//! femtovg rasterizes text over 92px from glyph outlines with a
//! stencil-then-cover fill. On a surface without a stencil buffer the cover
//! pass paints every glyph's bounding box, so a big clock comes out as solid
//! rectangles while small text still looks right. This renders a 160px "0"
//! and walks the row through its middle: a real glyph crosses ink twice (the
//! two sides of the bowl), a box crosses it once.
//!
//!   cargo build -p vigil-gl --example gl_large_text
//!   tests/gpu/run.sh -- target/debug/examples/gl_large_text

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle;
use slint::PhysicalSize;
use slint::platform::{Platform, WindowAdapter};
use vigil_gl::{GlContext, GlSurface, GlWindow};

const W: u32 = 320;
const H: u32 = 240;

const SCENE: &str = r#"
export component Probe inherits Window {
    background: #000000;
    Text {
        text: "0";
        color: #ffffff;
        font-size: 160px;
        width: parent.width;
        height: parent.height;
        horizontal-alignment: center;
        vertical-alignment: center;
    }
}
"#;

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
    let gbm = surface.window();

    let window = GlWindow::with_surface(surface, PhysicalSize::new(W, H)).expect("window");
    window.set_size(PhysicalSize::new(W, H));
    slint::platform::set_platform(Box::new(GlPlatform {
        window: RefCell::new(Some(window.clone())),
    }))
    .expect("set platform");

    let compiler = slint_interpreter::Compiler::default();
    let result = spin_on(compiler.build_from_source(SCENE.into(), Default::default()));
    for diagnostic in result.diagnostics() {
        eprintln!("scene: {diagnostic}");
    }
    let instance = result
        .component("Probe")
        .expect("Probe")
        .create()
        .expect("instantiate");
    instance.show().expect("show");
    window.render().expect("render");

    // SAFETY: a swap just happened, and the buffer is released below.
    let bo = unsafe { gbm.lock_front_buffer() }.expect("lock front buffer");
    let row = bo
        .map(0, H / 2, W, 1, |mapped| {
            // XRGB8888 little-endian: B, G, R, X. Green alone is enough to
            // tell white ink from a black background.
            mapped.buffer()[..W as usize * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|px| px[1] > 128)
                .collect::<Vec<bool>>()
        })
        .expect("map front buffer");
    gbm.release_buffer(bo);

    let runs = row.windows(2).filter(|w| !w[0] && w[1]).count() + usize::from(row[0]);
    let picture: String = row.iter().map(|&ink| if ink { '#' } else { '.' }).collect();
    println!("middle row: {picture}");
    assert!(
        runs == 2,
        "a 160px \"0\" should cross ink twice through its middle, crossed it {runs} times \
         (once means the glyph was filled as a box: no stencil buffer)"
    );
    println!("LARGE TEXT OK");
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
