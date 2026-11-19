#![deny(unsafe_code)]

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

pub struct GlContext {
    context: EGLContext,
    display: EGLDisplay,
    device: GbmDevice<Arc<OwnedFd>>,
}

impl GlContext {
    pub fn open(path: &Path) -> Result<Self, GlError> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| GlError(format!("open {}: {e}", path.display())))?;
        Self::from_fd(Arc::new(OwnedFd::from(file)))
    }

    #[allow(unsafe_code)]
    pub fn from_fd(fd: Arc<OwnedFd>) -> Result<Self, GlError> {
        let device = GbmDevice::new(fd).map_err(|e| GlError(format!("gbm device: {e}")))?;
        #[expect(
            clippy::undocumented_unsafe_blocks,
            reason = "unjustified: holds only while nothing else in the process creates or terminates EGL displays"
        )]
        let display =
            unsafe { EGLDisplay::new(device.clone()) }.map_err(|e| GlError(format!("egl: {e}")))?;
        let context =
            EGLContext::new(&display).map_err(|e| GlError(format!("egl context: {e}")))?;
        Ok(Self {
            context,
            display,
            device,
        })
    }

    pub fn gbm_device(&self) -> &GbmDevice<Arc<OwnedFd>> {
        &self.device
    }

    #[allow(unsafe_code)]
    pub fn make_current(&self) -> Result<(), GlError> {
        // SAFETY: a context still current on another thread makes eglMakeCurrent
        // fail with EGL_BAD_ACCESS, which is returned here, rather than bind twice.
        unsafe { self.context.make_current() }.map_err(|e| GlError(format!("make current: {e}")))
    }

    #[allow(unsafe_code)]
    fn config_for(&self, fourcc: u32) -> Result<ChosenConfig, GlError> {
        let handle = self.display.get_display_handle();
        let attrs = [
            ffi::egl::SURFACE_TYPE as ffi::egl::types::EGLint,
            ffi::egl::WINDOW_BIT as ffi::egl::types::EGLint,
            ffi::egl::RENDERABLE_TYPE as ffi::egl::types::EGLint,
            ffi::egl::OPENGL_ES2_BIT as ffi::egl::types::EGLint,
            ffi::egl::STENCIL_SIZE as ffi::egl::types::EGLint,
            MIN_STENCIL_BITS as ffi::egl::types::EGLint,
            ffi::egl::NONE as ffi::egl::types::EGLint,
        ];
        let mut configs: [ffi::egl::types::EGLConfig; 64] = [std::ptr::null(); 64];
        let mut found: ffi::egl::types::EGLint = 0;
        // SAFETY: the display handle is live for `self`, `attrs` is EGL_NONE
        // terminated, and EGL writes at most `configs.len()` entries and one count.
        let ok = unsafe {
            ffi::egl::ChooseConfig(
                handle.handle,
                attrs.as_ptr(),
                configs.as_mut_ptr(),
                configs.len() as ffi::egl::types::EGLint,
                &raw mut found,
            )
        };
        if ok == 0 || found == 0 {
            return Err(GlError("no window-capable EGL configs".into()));
        }
        let attrib = |config: ffi::egl::types::EGLConfig, name: ffi::egl::types::EGLenum| {
            let mut value: ffi::egl::types::EGLint = 0;
            // SAFETY: `config` came from ChooseConfig on this live display, and
            // EGL writes one EGLint into `value`.
            let ok = unsafe {
                ffi::egl::GetConfigAttrib(
                    handle.handle,
                    config,
                    name as ffi::egl::types::EGLint,
                    &raw mut value,
                )
            };
            (ok != 0).then_some(value)
        };
        for config in configs.iter().take(found as usize).copied() {
            if attrib(config, ffi::egl::NATIVE_VISUAL_ID).map(|id| id as u32) != Some(fourcc) {
                continue;
            }
            let stencil_bits = attrib(config, ffi::egl::STENCIL_SIZE).unwrap_or(0);
            if stencil_bits < MIN_STENCIL_BITS {
                continue;
            }
            return Ok(ChosenConfig {
                config,
                alpha_bits: attrib(config, ffi::egl::ALPHA_SIZE).unwrap_or(0) as u8,
                depth_bits: attrib(config, ffi::egl::DEPTH_SIZE).unwrap_or(0) as u8,
                stencil_bits: stencil_bits as u8,
            });
        }
        Err(GlError(format!(
            "no EGL config with native visual {fourcc:#x} and a {MIN_STENCIL_BITS}-bit stencil buffer (of {found})"
        )))
    }
}

const MIN_STENCIL_BITS: i32 = 8;

struct ChosenConfig {
    config: ffi::egl::types::EGLConfig,
    alpha_bits: u8,
    depth_bits: u8,
    stencil_bits: u8,
}

#[derive(Clone)]
pub struct GbmWindow(Arc<gbm::Surface<()>>);

#[allow(unsafe_code)]
#[expect(
    clippy::undocumented_unsafe_blocks,
    reason = "unjustified: a cloned GbmWindow on two threads can race gbm_surface calls"
)]
unsafe impl Send for GbmWindow {}

// SAFETY: `create` hands EGL the gbm_surface this Arc keeps alive for as long
// as the EGLSurface holds a clone of the window.
#[allow(unsafe_code)]
unsafe impl EGLNativeSurface for GbmWindow {
    unsafe fn create(
        &self,
        display: &Arc<EGLDisplayHandle>,
        config_id: ffi::egl::types::EGLConfig,
    ) -> Result<*const c_void, smithay::backend::egl::EGLError> {
        // SAFETY: the caller guarantees `config_id` is a valid config for
        // `display`, and the gbm_surface pointer is live for `self`.
        smithay::backend::egl::wrap_egl_call_ptr(|| unsafe {
            ffi::egl::CreatePlatformWindowSurfaceEXT(
                display.handle,
                config_id,
                self.0.as_raw().cast_mut().cast(),
                std::ptr::null(),
            )
        })
    }

    fn identifier(&self) -> Option<String> {
        Some("vigil/GBM".into())
    }
}

impl GbmWindow {
    /// # Safety
    /// Only valid after a swap, and the returned buffer must be released back
    /// to the surface.
    #[allow(unsafe_code)]
    pub unsafe fn lock_front_buffer(&self) -> Result<gbm::BufferObject<()>, GlError> {
        // SAFETY: forwarded; the caller upholds gbm's once-per-swap contract.
        unsafe { self.0.lock_front_buffer() }
            .map_err(|e| GlError(format!("lock front buffer: {e}")))
    }

    pub fn release_buffer(&self, bo: gbm::BufferObject<()>) {
        drop(bo);
    }
}

pub struct GlSurface {
    context: Rc<GlContext>,
    surface: EGLSurface,
    window: GbmWindow,
}

impl GlSurface {
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

    #[allow(unsafe_code)]
    fn with_format(
        context: &Rc<GlContext>,
        width: u32,
        height: u32,
        fourcc: u32,
        format: gbm::Format,
    ) -> Result<Self, GlError> {
        let chosen = context.config_for(fourcc)?;
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
        let pixel_format = PixelFormat {
            hardware_accelerated: true,
            color_bits: 24,
            alpha_bits: chosen.alpha_bits,
            depth_bits: chosen.depth_bits,
            stencil_bits: chosen.stencil_bits,
            stereoscopy: false,
            multisampling: None,
            srgb: false,
        };
        // SAFETY: `chosen.config` came from ChooseConfig on this same display.
        let surface = unsafe {
            EGLSurface::new(
                &context.display,
                pixel_format,
                chosen.config,
                window.clone(),
            )
        }
        .map_err(|e| GlError(format!("egl window surface: {e}")))?;
        let context = context.clone();
        Ok(Self {
            context,
            surface,
            window,
        })
    }

    pub fn window(&self) -> GbmWindow {
        self.window.clone()
    }
}

// SAFETY: get_proc_address forwards to eglGetProcAddress, whose pointers are
// context independent under EGL 1.5 and live as long as libEGL stays loaded.
#[allow(unsafe_code)]
unsafe impl OpenGLInterface for GlSurface {
    fn ensure_current(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // SAFETY: a context still current on another thread makes eglMakeCurrent
        // fail with EGL_BAD_ACCESS, which is returned here, rather than bind twice.
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

    fn resize(
        &self,
        _width: NonZeroU32,
        _height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    fn get_proc_address(&self, name: &CStr) -> *const c_void {
        match name.to_str() {
            // SAFETY: `self` owns a GlContext, so libEGL is loaded, and EGL 1.5
            // function pointers do not depend on which context is current.
            Ok(name) => unsafe { get_proc_address(name) },
            Err(_) => std::ptr::null(),
        }
    }
}

struct Surfaceless(GlContext);

// SAFETY: get_proc_address forwards to eglGetProcAddress, whose pointers are
// context independent under EGL 1.5 and live as long as libEGL stays loaded.
#[allow(unsafe_code)]
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
            // SAFETY: `self` owns a GlContext, so libEGL is loaded, and EGL 1.5
            // function pointers do not depend on which context is current.
            Ok(name) => unsafe { get_proc_address(name) },
            Err(_) => std::ptr::null(),
        }
    }
}

pub struct GlWindow {
    window: Window,
    renderer: FemtoVGRenderer,
    size: std::cell::Cell<PhysicalSize>,
    needs_redraw: std::cell::Cell<bool>,
    redraw: std::cell::RefCell<Option<slint_idle_runtime::RedrawHandle<vigil_core::OutputId>>>,
    gbm: Option<GbmWindow>,
}

impl GlWindow {
    pub fn with_surface(surface: GlSurface, size: PhysicalSize) -> Result<Rc<Self>, GlError> {
        let gbm = surface.window();
        surface
            .ensure_current()
            .map_err(|e| GlError(format!("make current: {e}")))?;
        Self::build(FemtoVGRenderer::new(surface), size, Some(gbm))
    }

    pub fn new(context: GlContext, size: PhysicalSize) -> Result<Rc<Self>, GlError> {
        context.make_current()?;
        Self::build(FemtoVGRenderer::new(Surfaceless(context)), size, None)
    }

    fn build(
        renderer: Result<FemtoVGRenderer, slint::PlatformError>,
        size: PhysicalSize,
        gbm: Option<GbmWindow>,
    ) -> Result<Rc<Self>, GlError> {
        let renderer = renderer.map_err(|e| GlError(format!("femtovg: {e}")))?;
        Ok(Rc::new_cyclic(|weak: &std::rc::Weak<Self>| Self {
            window: Window::new(weak.clone()),
            renderer,
            size: std::cell::Cell::new(size),
            needs_redraw: std::cell::Cell::new(true),
            redraw: std::cell::RefCell::new(None),
            gbm,
        }))
    }

    pub fn set_redraw_handle(
        &self,
        redraw: slint_idle_runtime::RedrawHandle<vigil_core::OutputId>,
    ) {
        redraw.request_redraw();
        *self.redraw.borrow_mut() = Some(redraw);
    }

    pub fn render(&self) -> Result<(), GlError> {
        self.renderer
            .render()
            .map_err(|e| GlError(format!("render: {e}")))
    }

    pub fn take_needs_redraw(&self) -> bool {
        self.needs_redraw.replace(false)
    }

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

pub struct GlBackend {
    window: Rc<GlWindow>,
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
        let scene_dirty = self.window.take_needs_redraw();
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
