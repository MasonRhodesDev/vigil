//! EGL/GBM context, window surface, and a Slint window adapter for the GL
//! rendering path (DESIGN.md §7, issue #17).
//!
//! Software rendering stays the baseline and the default. This crate exists
//! for fidelity headroom -- gradients and effects the theme contract cannot
//! ask for today -- and is never linked into the shipped greeter unless the
//! `gl` feature is turned on.
//!
//! # Shape
//!
//! femtovg draws into the *default* framebuffer, which exists only when the
//! context is current with a real EGL window surface. That surface is backed
//! by a GBM surface, whose front buffer -- locked after each swap -- is the
//! buffer object a DRM framebuffer is created from. So the chain is:
//!
//! ```text
//!   GBM device -> EGL display -> EGL context
//!                             -> GBM surface -> EGL window surface
//!                                            -> (after swap) front buffer -> DRM fb
//! ```
//!
//! smithay implements `EGLNativeSurface` for Xlib and Wayland but not GBM, so
//! [`GbmWindow`] supplies it.
//!
//! # Testing
//!
//! EGL rendering on a card node needs DRM master, which a desktop's
//! compositor holds. `tests/gpu/run.sh` runs things in a VM that owns its own
//! GPU -- with no host GPU required, Mesa falling back to kms_swrast.
//!
//!   tests/gpu/run.sh -- target/debug/examples/gl_scanout
//!   tests/gpu/run.sh --accel -- target/debug/examples/gl_scanout
//!
//! What is still missing is the CRTC commit: turning the locked front buffer
//! into a DRM framebuffer and flipping to it, which is the presenter's job.

use std::ffi::{CStr, c_void};
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use gbm::AsRaw;
use slint::platform::femtovg_renderer::{FemtoVGRenderer, OpenGLInterface};
use slint::platform::{Renderer, WindowAdapter};
use slint::{PhysicalSize, Window};

/// Re-exported so consumers can size a window without depending on slint.
pub use gbm::{BufferObjectFlags as GbmBufferFlags, Format as GbmFormat};
pub use slint::PhysicalSize as PhysicalSizeExport;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::egl::display::EGLDisplayHandle;
use smithay::backend::egl::display::PixelFormat;
use smithay::backend::egl::native::EGLNativeSurface;
use smithay::backend::egl::{EGLContext, EGLDisplay, EGLSurface, ffi, get_proc_address};

#[derive(Debug)]
pub struct GlError(pub String);

impl std::fmt::Display for GlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GlError {}

/// An EGL context on a GBM device.
pub struct GlContext {
    context: EGLContext,
    display: EGLDisplay,
    /// `Arc<OwnedFd>` rather than `OwnedFd`: the display takes ownership of a
    /// device, and surfaces need one too, so the fd has to be shareable.
    device: GbmDevice<Arc<OwnedFd>>,
}

impl GlContext {
    /// Open a DRM node and build a context on it.
    ///
    /// A *render* node needs no DRM master, which is enough to build a
    /// context and compile a scene. Producing pixels needs a card node,
    /// because that is what can carry a window surface.
    pub fn open(path: &Path) -> Result<Self, GlError> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| GlError(format!("open {}: {e}", path.display())))?;
        Self::from_fd(Arc::new(OwnedFd::from(file)))
    }

    /// Build a context on an already-open node.
    ///
    /// Scanout needs this: DRM master is granted per *open file description*,
    /// so the modesetting device and the GBM device have to share one --
    /// duplicated, not opened a second time. A second `open` of the same card
    /// is a different description and does not get master.
    pub fn from_fd(fd: Arc<OwnedFd>) -> Result<Self, GlError> {
        let device = GbmDevice::new(fd).map_err(|e| GlError(format!("gbm device: {e}")))?;
        // SAFETY: the display keeps its own reference to the device's fd.
        let display =
            unsafe { EGLDisplay::new(device.clone()) }.map_err(|e| GlError(format!("egl: {e}")))?;
        // A *configless* context (EGL_KHR_no_config_context), so the surface
        // picks its own config below. Letting smithay choose one here does
        // not work: its requirements are minimums, so "at least 24-bit
        // colour" happily selects a 10-bit-per-channel XR30 config, and
        // virtio-gpu refuses to scan that out -- AddFB2 fails with ENOENT.
        let context =
            EGLContext::new(&display).map_err(|e| GlError(format!("egl context: {e}")))?;
        Ok(Self {
            context,
            display,
            device,
        })
    }

    /// The GBM device the context allocates on — for callers that need a
    /// scanout-class buffer of their own (the rotation acceptance test
    /// allocates one to ask the display hardware before committing to GL).
    pub fn gbm_device(&self) -> &GbmDevice<Arc<OwnedFd>> {
        &self.device
    }

    /// Make the context current with no surface bound. Rendering then targets
    /// whatever framebuffer object the caller binds; there is no default one.
    pub fn make_current(&self) -> Result<(), GlError> {
        // SAFETY: called on the thread that owns the context, and Slint's
        // renderer is single-threaded.
        unsafe { self.context.make_current() }.map_err(|e| GlError(format!("make current: {e}")))
    }

    /// An EGL config whose native visual is exactly `fourcc`.
    ///
    /// The config and the GBM surface must agree on format or
    /// `eglCreateWindowSurface` returns `EGL_BAD_MATCH`, and the format also
    /// has to be one the display hardware will scan out. Both constraints
    /// point at the same answer: ask for the format by name rather than
    /// describing it in bit counts and taking what turns up.
    fn config_for(&self, fourcc: u32) -> Result<ffi::egl::types::EGLConfig, GlError> {
        let handle = self.display.get_display_handle();
        let attrs = [
            ffi::egl::SURFACE_TYPE as ffi::egl::types::EGLint,
            ffi::egl::WINDOW_BIT as ffi::egl::types::EGLint,
            ffi::egl::RENDERABLE_TYPE as ffi::egl::types::EGLint,
            ffi::egl::OPENGL_ES2_BIT as ffi::egl::types::EGLint,
            ffi::egl::NONE as ffi::egl::types::EGLint,
        ];
        let mut configs = [std::ptr::null(); 64];
        let mut found: ffi::egl::types::EGLint = 0;
        // SAFETY: a valid display, a NONE-terminated attribute list, and a
        // buffer matching the count we pass.
        let ok = unsafe {
            ffi::egl::ChooseConfig(
                handle.handle,
                attrs.as_ptr(),
                configs.as_mut_ptr() as *mut _,
                configs.len() as ffi::egl::types::EGLint,
                &mut found,
            )
        };
        if ok == 0 || found == 0 {
            return Err(GlError("no window-capable EGL configs".into()));
        }
        for config in configs.iter().take(found as usize) {
            let mut id: ffi::egl::types::EGLint = 0;
            // SAFETY: a config just returned by ChooseConfig on this display.
            let ok = unsafe {
                ffi::egl::GetConfigAttrib(
                    handle.handle,
                    *config,
                    ffi::egl::NATIVE_VISUAL_ID as ffi::egl::types::EGLint,
                    &mut id,
                )
            };
            if ok != 0 && id as u32 == fourcc {
                return Ok(*config);
            }
        }
        Err(GlError(format!(
            "no EGL config with native visual {fourcc:#x} (of {found})"
        )))
    }
}

/// A GBM surface usable as an EGL native window.
///
/// Shared: the EGL surface owns one handle and the caller keeps another to
/// lock the front buffer after each swap.
#[derive(Clone)]
pub struct GbmWindow(Arc<gbm::Surface<()>>);

// SAFETY: the trait demands Send. The surface is only ever touched from the
// thread that owns the context -- Slint's renderer is single-threaded -- and
// the pointer inside is valid for as long as the Arc keeps the surface alive.
unsafe impl Send for GbmWindow {}
unsafe impl Sync for GbmWindow {}

// SAFETY: `create` returns a surface made from this window's own live GBM
// surface pointer, for the config the caller supplies.
unsafe impl EGLNativeSurface for GbmWindow {
    unsafe fn create(
        &self,
        display: &Arc<EGLDisplayHandle>,
        config_id: ffi::egl::types::EGLConfig,
    ) -> Result<*const c_void, smithay::backend::egl::EGLError> {
        smithay::backend::egl::wrap_egl_call_ptr(|| unsafe {
            ffi::egl::CreatePlatformWindowSurfaceEXT(
                display.handle,
                config_id,
                self.0.as_raw() as *mut _,
                std::ptr::null(),
            )
        })
    }

    fn identifier(&self) -> Option<String> {
        Some("vigil/GBM".into())
    }
}

impl GbmWindow {
    /// The most recently swapped buffer. This is the object a DRM framebuffer
    /// is created from, and it must be released once the CRTC is done with
    /// it or the surface runs out of buffers to render into.
    ///
    /// # Safety
    /// Only valid after a swap, and the returned buffer must be released back
    /// to the surface.
    pub unsafe fn lock_front_buffer(&self) -> Result<gbm::BufferObject<()>, GlError> {
        unsafe { self.0.lock_front_buffer() }
            .map_err(|e| GlError(format!("lock front buffer: {e}")))
    }

    pub fn release_buffer(&self, bo: gbm::BufferObject<()>) {
        drop(bo);
    }
}

/// An EGL window surface over a GBM surface: what gives femtovg a default
/// framebuffer to draw into.
pub struct GlSurface {
    context: Rc<GlContext>,
    surface: EGLSurface,
    window: GbmWindow,
}

impl GlSurface {
    /// Scanout formats in preference order. XRGB8888 first: it is what every
    /// display pipeline handles, whereas the 10-bit variants are accepted by
    /// EGL and then rejected by `AddFB2` on hardware that cannot scan them
    /// out (virtio-gpu fails with ENOENT).
    const FORMATS: &'static [(u32, gbm::Format)] = &[
        (0x3432_5258, gbm::Format::Xrgb8888),
        (0x3330_3358, gbm::Format::Xrgb2101010),
    ];

    pub fn new(context: Rc<GlContext>, width: u32, height: u32) -> Result<Self, GlError> {
        let mut last = GlError("no candidate formats".into());
        for (fourcc, format) in Self::FORMATS {
            match Self::with_format(&context, width, height, *fourcc, *format) {
                Ok(surface) => return Ok(surface),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn with_format(
        context: &Rc<GlContext>,
        width: u32,
        height: u32,
        fourcc: u32,
        format: gbm::Format,
    ) -> Result<Self, GlError> {
        let config = context.config_for(fourcc)?;
        let gbm_surface = context
            .device
            .create_surface::<()>(
                width,
                height,
                format,
                gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|e| GlError(format!("gbm surface: {e}")))?;
        let window = GbmWindow(Arc::new(gbm_surface));
        // The context is configless, so it carries no pixel format of its
        // own; describe the one the chosen config actually is.
        let pixel_format = PixelFormat {
            hardware_accelerated: true,
            color_bits: 24,
            alpha_bits: 0,
            depth_bits: 0,
            stencil_bits: 0,
            stereoscopy: false,
            multisampling: None,
            srgb: false,
        };
        // SAFETY: `config` came from this display's own ChooseConfig, and the
        // GBM surface was created with the format that config names.
        let surface =
            unsafe { EGLSurface::new(&context.display, pixel_format, config, window.clone()) }
                .map_err(|e| GlError(format!("egl window surface: {e}")))?;
        let context = context.clone();
        Ok(Self {
            context,
            surface,
            window,
        })
    }

    /// A handle for locking the front buffer after a swap.
    pub fn window(&self) -> GbmWindow {
        self.window.clone()
    }
}

// SAFETY: every method forwards to EGL on the owning thread. `ensure_current`
// binds the window surface, which is what makes the default framebuffer exist.
unsafe impl OpenGLInterface for GlSurface {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // SAFETY: single-threaded renderer; surface and context share a display.
        unsafe {
            self.context
                .context
                .make_current_with_surface(&self.surface)
        }
        .map_err(|e| Box::new(GlError(e.to_string())) as Box<_>)
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.surface
            .swap_buffers(None)
            .map_err(|e| Box::new(GlError(e.to_string())) as Box<_>)
    }

    /// GBM surfaces are fixed size: a resize means a new surface, which is a
    /// new scanout buffer chain, so the presenter rebuilds rather than
    /// resizing in place.
    fn resize(
        &self,
        _width: NonZeroU32,
        _height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn get_proc_address(&self, name: &CStr) -> *const c_void {
        match name.to_str() {
            // SAFETY: plain eglGetProcAddress; a missing symbol comes back
            // null, which is what the caller expects.
            Ok(name) => unsafe { get_proc_address(name) },
            Err(_) => std::ptr::null(),
        }
    }
}

/// A context with no window surface, for the scene-only path.
///
/// Rendering through this reaches no default framebuffer and quietly produces
/// black, so it is only good for proving a theme compiles and instantiates.
struct Surfaceless(GlContext);

// SAFETY: forwards to EGL on the owning thread; swapping is meaningless
// without a surface and is a no-op rather than an error.
unsafe impl OpenGLInterface for Surfaceless {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0
            .make_current()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    fn swap_buffers(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn resize(
        &self,
        _width: NonZeroU32,
        _height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn get_proc_address(&self, name: &CStr) -> *const c_void {
        match name.to_str() {
            // SAFETY: plain eglGetProcAddress; missing symbols come back null.
            Ok(name) => unsafe { get_proc_address(name) },
            Err(_) => std::ptr::null(),
        }
    }
}

/// A Slint window backed by the FemtoVG GL renderer.
///
/// The software path's equivalent is Slint's `MinimalSoftwareWindow`; there
/// is no ready-made GL counterpart, so this is the minimum a `WindowAdapter`
/// has to provide.
pub struct GlWindow {
    window: Window,
    renderer: FemtoVGRenderer,
    size: std::cell::Cell<PhysicalSize>,
    /// Set by Slint when the scene needs drawing again. Without consulting
    /// it a GL greeter would redraw and flip forever on an idle login
    /// screen; the software path gets the same signal from `draw_if_needed`.
    needs_redraw: std::cell::Cell<bool>,
    redraw: std::cell::RefCell<Option<hypr_slint_runtime::RedrawHandle<vigil_core::OutputId>>>,
    /// Present when rendering on-screen; absent for a context-only window,
    /// which can compile and instantiate a scene but not produce pixels.
    gbm: Option<GbmWindow>,
}

impl GlWindow {
    /// A window that renders into `surface`'s GBM buffers.
    pub fn with_surface(surface: GlSurface, size: PhysicalSize) -> Result<Rc<Self>, GlError> {
        let gbm = surface.window();
        surface
            .ensure_current()
            .map_err(|e| GlError(format!("make current: {e}")))?;
        Self::build(FemtoVGRenderer::new(surface), size, Some(gbm))
    }

    /// A window with no surface. It can compile and instantiate a scene, but
    /// rendering has no default framebuffer to land in and silently produces
    /// black -- useful only for checking that a theme loads.
    pub fn new(context: GlContext, size: PhysicalSize) -> Result<Rc<Self>, GlError> {
        // The renderer loads GL entry points while constructing, which needs a
        // context already current -- otherwise it panics inside glow reading
        // GL_VERSION rather than returning an error.
        context.make_current()?;
        Self::build(FemtoVGRenderer::new(Surfaceless(context)), size, None)
    }

    fn build(
        renderer: Result<FemtoVGRenderer, slint::PlatformError>,
        size: PhysicalSize,
        gbm: Option<GbmWindow>,
    ) -> Result<Rc<Self>, GlError> {
        let renderer = renderer.map_err(|e| GlError(format!("femtovg: {e}")))?;
        // The annotation matters: without it inference collapses `Self` into
        // `dyn WindowAdapter` and the unsize coercion never happens.
        Ok(Rc::new_cyclic(|weak: &std::rc::Weak<Self>| Self {
            window: Window::new(weak.clone()),
            renderer,
            size: std::cell::Cell::new(size),
            // Draw the first frame unconditionally.
            needs_redraw: std::cell::Cell::new(true),
            redraw: std::cell::RefCell::new(None),
            gbm,
        }))
    }

    pub fn set_redraw_handle(
        &self,
        redraw: hypr_slint_runtime::RedrawHandle<vigil_core::OutputId>,
    ) {
        redraw.request_redraw();
        *self.redraw.borrow_mut() = Some(redraw);
    }

    /// Draw the current scene and swap. The swapped buffer is then available
    /// from [`Self::gbm`] for the presenter to scan out.
    pub fn render(&self) -> Result<(), GlError> {
        self.renderer
            .render()
            .map_err(|e| GlError(format!("render: {e}")))
    }

    /// Whether Slint has asked for a redraw since this was last called.
    /// Reading it clears it.
    pub fn take_needs_redraw(&self) -> bool {
        self.needs_redraw.replace(false)
    }

    /// The GBM surface, when this window renders on-screen.
    pub fn gbm(&self) -> Option<&GbmWindow> {
        self.gbm.as_ref()
    }

    pub fn set_size(&self, size: PhysicalSize) {
        self.size.set(size);
        self.window
            .dispatch_event(slint::platform::WindowEvent::Resized {
                size: size.to_logical(self.window.scale_factor()),
            });
    }
}

impl WindowAdapter for GlWindow {
    fn window(&self) -> &Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn request_redraw(&self) {
        self.needs_redraw.set(true);
        if let Some(redraw) = self.redraw.borrow().as_ref() {
            redraw.request_redraw();
        }
    }
}

/// A [`RenderBackend`](vigil_core::RenderBackend) drawing through GL.
///
/// The counterpart to vigil-ui's software backend: same scene, same trait,
/// different way of turning it into pixels. The scene itself -- every
/// property, the pointer, the auth state -- is shared code neither backend
/// knows about.
pub struct GlBackend {
    window: Rc<GlWindow>,
    /// What the scene looked like when it was last presented. GL has no
    /// equivalent of the software path's partial-repaint bookkeeping, so
    /// without this an idle login screen would render and flip forever.
    last: Option<vigil_core::SceneView>,
    force: bool,
}

impl GlBackend {
    pub fn new(window: Rc<GlWindow>) -> Self {
        Self {
            window,
            last: None,
            force: true,
        }
    }

    /// The GBM surface being rendered into, for the presenter to scan out.
    pub fn gbm(&self) -> Option<&GbmWindow> {
        self.window.gbm()
    }
}

impl vigil_core::RenderBackend for GlBackend {
    fn request_present(&mut self) {
        self.force = true;
    }

    fn render(&mut self, view: &vigil_core::SceneView, canvas: vigil_core::Canvas<'_>) -> bool {
        if !matches!(canvas, vigil_core::Canvas::Gl { .. }) {
            eprintln!("vigil-gl: GL backend given a CPU canvas");
            return false;
        }
        // Slint's own redraw request fires once for a custom adapter and
        // never re-arms, so it cannot be the only signal; the view's revision
        // is what actually tracks scene changes. Both are honoured.
        let scene_dirty = self.window.take_needs_redraw();
        // The pointer is part of the view only when the scene composites a
        // cursor from it; with a hardware cursor (#25) motion must not look
        // like a scene change or GL re-renders per pixel travelled.
        let mut key = *view;
        if !key.cursor_visible {
            key.pointer = (0.0, 0.0);
        }
        if std::env::var_os("VIGIL_DEBUG_FRAMES").is_some() {
            eprintln!(
                "vigil-gl: dirty={scene_dirty} force={} view_changed={}",
                self.force,
                self.last != Some(key)
            );
        }
        if !self.force && !scene_dirty && self.last == Some(key) {
            return false;
        }
        if let Err(e) = self.window.render() {
            eprintln!("vigil-gl: render: {e}");
            return false;
        }
        self.last = Some(key);
        self.force = false;
        true
    }
}
