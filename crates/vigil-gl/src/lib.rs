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
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::egl::context::{GlAttributes, PixelFormatRequirements};
use smithay::backend::egl::display::EGLDisplayHandle;
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
        let device = GbmDevice::new(Arc::new(OwnedFd::from(file)))
            .map_err(|e| GlError(format!("gbm device: {e}")))?;
        // SAFETY: the display keeps its own reference to the device's fd.
        let display =
            unsafe { EGLDisplay::new(device.clone()) }.map_err(|e| GlError(format!("egl: {e}")))?;
        // `EGLContext::new` builds a *configless* context, whose config_id is
        // EGL_NO_CONFIG -- there is then nothing to query for a native visual
        // and nothing for a window surface to match. A window surface needs a
        // real config, so ask for one: 24-bit colour and no alpha, matching
        // the XRGB the scanout path wants.
        let context = EGLContext::new_with_config(
            &display,
            GlAttributes {
                version: (2, 0),
                profile: None,
                debug: cfg!(debug_assertions),
                vsync: false,
            },
            PixelFormatRequirements {
                hardware_accelerated: None,
                color_bits: Some(24),
                float_color_buffer: false,
                alpha_bits: Some(0),
                depth_bits: None,
                stencil_bits: None,
                multisampling: None,
            },
        )
        .map_err(|e| GlError(format!("egl context: {e}")))?;
        Ok(Self {
            context,
            display,
            device,
        })
    }

    /// Make the context current with no surface bound. Rendering then targets
    /// whatever framebuffer object the caller binds; there is no default one.
    pub fn make_current(&self) -> Result<(), GlError> {
        // SAFETY: called on the thread that owns the context, and Slint's
        // renderer is single-threaded.
        unsafe { self.context.make_current() }.map_err(|e| GlError(format!("make current: {e}")))
    }

    /// The fourcc this context's EGL config expects a native window to be.
    ///
    /// Reading it and *then* creating the GBM surface to match is what keeps
    /// the two in agreement. Choosing a format first and hoping the config
    /// matches is the classic way to earn `EGL_BAD_MATCH` from
    /// `eglCreateWindowSurface`: the first config offered is typically
    /// ARGB8888 against an XRGB8888 surface.
    pub fn native_visual_fourcc(&self) -> Result<u32, GlError> {
        let handle = self.display.get_display_handle();
        let mut id: ffi::egl::types::EGLint = 0;
        // SAFETY: a valid display handle and the config this context was
        // created with; NATIVE_VISUAL_ID is defined for every config.
        let ok = unsafe {
            ffi::egl::GetConfigAttrib(
                handle.handle,
                self.context.config_id(),
                ffi::egl::NATIVE_VISUAL_ID as ffi::egl::types::EGLint,
                &mut id,
            )
        };
        if ok == 0 {
            return Err(GlError("could not read EGL_NATIVE_VISUAL_ID".into()));
        }
        Ok(id as u32)
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
    pub fn new(context: Rc<GlContext>, width: u32, height: u32) -> Result<Self, GlError> {
        let fourcc = context.native_visual_fourcc()?;
        let format = gbm::Format::try_from(fourcc)
            .map_err(|_| GlError(format!("unknown native visual fourcc {fourcc:#x}")))?;
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
        let pixel_format = context
            .context
            .pixel_format()
            .ok_or_else(|| GlError("context has no pixel format".into()))?;
        // SAFETY: the config comes from this very context.
        let surface = unsafe {
            EGLSurface::new(
                &context.display,
                pixel_format,
                context.context.config_id(),
                window.clone(),
            )
        }
        .map_err(|e| GlError(format!("egl window surface: {e}")))?;
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
            gbm,
        }))
    }

    /// Draw the current scene and swap. The swapped buffer is then available
    /// from [`Self::gbm`] for the presenter to scan out.
    pub fn render(&self) -> Result<(), GlError> {
        self.renderer
            .render()
            .map_err(|e| GlError(format!("render: {e}")))
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
}
