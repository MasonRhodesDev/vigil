//! EGL/GBM context and a Slint window adapter for the GL rendering path
//! (DESIGN.md §7, issue #17).
//!
//! Software rendering stays the baseline and the default. This crate exists
//! for fidelity headroom -- gradients and effects the theme contract cannot
//! ask for today -- and is never linked into the shipped greeter unless the
//! `gl` feature is turned on.
//!
//! # State
//!
//! The context and window adapter below are complete and exercised. What is
//! *not* here yet is scanout: femtovg draws into the default framebuffer,
//! which exists only once there is an EGL window surface over a GBM surface.
//! smithay implements `EGLNativeSurface` for Xlib and Wayland but not GBM, so
//! that glue has to be written before a frame can reach a CRTC. Until then
//! this crate builds a context and a scene but presents nothing.

use std::ffi::{CStr, c_void};
use std::num::NonZeroU32;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;

use slint::platform::femtovg_renderer::{FemtoVGRenderer, OpenGLInterface};
use slint::platform::{Renderer, WindowAdapter};
use slint::{PhysicalSize, Window};
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::egl::{EGLContext, EGLDisplay, get_proc_address};

#[derive(Debug)]
pub struct GlError(pub String);

impl std::fmt::Display for GlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GlError {}

/// An EGL context on a GBM device.
///
/// Field order is drop order: the context must go before the display, and
/// the display before the device that backs it.
pub struct GlContext {
    context: EGLContext,
    /// Owns the GBM device it was built from; both outlive the context.
    _display: EGLDisplay,
}

impl GlContext {
    /// Open a DRM node and build a context on it.
    ///
    /// A *render* node (`/dev/dri/renderD*`) needs no DRM master and no seat,
    /// which is what makes this testable headlessly -- the same path the
    /// greeter will use on a card node, minus scanout.
    pub fn open(path: &Path) -> Result<Self, GlError> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| GlError(format!("open {}: {e}", path.display())))?;
        let gbm =
            GbmDevice::new(OwnedFd::from(file)).map_err(|e| GlError(format!("gbm device: {e}")))?;
        // SAFETY: `new` takes ownership of the device and keeps it alive for
        // the display's lifetime.
        let display = unsafe { EGLDisplay::new(gbm) }.map_err(|e| GlError(format!("egl: {e}")))?;
        let context =
            EGLContext::new(&display).map_err(|e| GlError(format!("egl context: {e}")))?;
        Ok(Self {
            context,
            _display: display,
        })
    }

    /// Make the context current with no surface bound. Requires
    /// `EGL_KHR_surfaceless_context`; rendering then targets whatever
    /// framebuffer object the caller binds.
    pub fn make_current(&self) -> Result<(), GlError> {
        // SAFETY: called on the thread that owns the context, and Slint's
        // renderer is single-threaded.
        unsafe { self.context.make_current() }.map_err(|e| GlError(format!("make current: {e}")))
    }
}

// SAFETY: every method below either forwards to EGL on the owning thread or
// is a no-op. The contract's requirement is that the context is current when
// `ensure_current` returns, which `make_current` guarantees.
unsafe impl OpenGLInterface for GlContext {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.make_current().map_err(|e| e.0.into())
    }

    /// Surfaceless: there is no swapchain to present. The scanout path
    /// swaps by locking the GBM front buffer and committing it to the CRTC,
    /// which belongs to the presenter, not here.
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
            // SAFETY: plain eglGetProcAddress; a missing symbol comes back
            // null, which is what the caller expects.
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
}

impl GlWindow {
    /// Build a window rendering through `context`.
    pub fn new(context: GlContext, size: PhysicalSize) -> Result<Rc<Self>, GlError> {
        // The renderer loads GL entry points while constructing, which needs a
        // context already current -- otherwise it panics inside glow reading
        // GL_VERSION rather than returning an error.
        context.make_current()?;
        let renderer =
            FemtoVGRenderer::new(context).map_err(|e| GlError(format!("femtovg: {e}")))?;
        // The annotation matters: without it inference collapses `Self` into
        // `dyn WindowAdapter` and the unsize coercion never happens.
        Ok(Rc::new_cyclic(|weak: &std::rc::Weak<Self>| Self {
            window: Window::new(weak.clone()),
            renderer,
            size: std::cell::Cell::new(size),
        }))
    }

    /// Draw the current scene into the bound framebuffer.
    pub fn render(&self) -> Result<(), GlError> {
        self.renderer
            .render()
            .map_err(|e| GlError(format!("render: {e}")))
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
