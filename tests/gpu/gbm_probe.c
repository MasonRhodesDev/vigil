// Environment check for the GL path (issue #17): can we get an EGL *window*
// surface over a GBM surface, render into it, and lock the result as a
// buffer object? That last object is what becomes a DRM framebuffer, so this
// is the whole scanout path minus the commit.
//
// Run it through the GPU harness, which supplies a card node we are master
// on -- on a desktop the compositor holds master and this fails:
//
//   cc -O1 -o /tmp/gbm_probe tests/gpu/gbm_probe.c $(pkg-config --cflags --libs gbm egl glesv2)
//   tests/gpu/run.sh -- /tmp/gbm_probe /dev/dri/card0
//
// Two mistakes it exists to document, because both fail quietly rather than
// loudly: the EGLConfig must match the GBM surface's format by
// EGL_NATIVE_VISUAL_ID (otherwise EGL_BAD_MATCH), and pixels must be read
// before eglSwapBuffers (afterwards the back buffer is undefined and reads
// black however well the draw went).
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
    const char *node = argc > 1 ? argv[1] : "/dev/dri/card1";
    int fd = open(node, O_RDWR);
    if (fd < 0) { printf("FAIL open %s\n", node); return 1; }

    struct gbm_device *dev = gbm_create_device(fd);
    if (!dev) { printf("FAIL gbm_create_device\n"); return 1; }
    printf("gbm device on %s: ok (backend %s)\n", node, gbm_device_get_backend_name(dev));

    struct gbm_surface *gs = gbm_surface_create(dev, 640, 480, GBM_FORMAT_XRGB8888,
                                                GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING);
    if (!gs) {
        printf("  scanout+rendering surface failed; retrying rendering-only\n");
        gs = gbm_surface_create(dev, 640, 480, GBM_FORMAT_XRGB8888, GBM_BO_USE_RENDERING);
    }
    if (!gs) { printf("FAIL gbm_surface_create\n"); return 1; }
    printf("gbm surface: ok\n");

    PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay dpy = getdpy ? getdpy(EGL_PLATFORM_GBM_KHR, dev, NULL) : EGL_NO_DISPLAY;
    if (dpy == EGL_NO_DISPLAY) { printf("FAIL eglGetPlatformDisplay\n"); return 1; }
    EGLint major, minor;
    if (!eglInitialize(dpy, &major, &minor)) { printf("FAIL eglInitialize\n"); return 1; }
    printf("egl %d.%d: ok\n", major, minor);

    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint attrs[] = {EGL_SURFACE_TYPE, EGL_WINDOW_BIT, EGL_RENDERABLE_TYPE,
                      EGL_OPENGL_ES2_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8,
                      EGL_BLUE_SIZE, 8, EGL_NONE};
    // The config's EGL_NATIVE_VISUAL_ID must equal the GBM surface's format,
    // or eglCreateWindowSurface returns EGL_BAD_MATCH. Taking the first
    // config is the classic way to get this wrong: it is usually ARGB8888
    // against an XRGB8888 surface.
    EGLConfig cfgs[64]; EGLint n = 0;
    if (!eglChooseConfig(dpy, attrs, cfgs, 64, &n) || n < 1) { printf("FAIL eglChooseConfig\n"); return 1; }
    EGLConfig cfg = NULL;
    for (EGLint i = 0; i < n; i++) {
        EGLint vid = 0;
        eglGetConfigAttrib(dpy, cfgs[i], EGL_NATIVE_VISUAL_ID, &vid);
        if (vid == (EGLint)GBM_FORMAT_XRGB8888) { cfg = cfgs[i]; break; }
    }
    if (!cfg) { printf("FAIL no config matching GBM_FORMAT_XRGB8888 (of %d)\n", n); return 1; }
    printf("matched egl config to gbm format (of %d candidates)\n", n);

    EGLint ctxattrs[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxattrs);
    if (ctx == EGL_NO_CONTEXT) { printf("FAIL eglCreateContext\n"); return 1; }

    EGLSurface surf = eglCreateWindowSurface(dpy, cfg, (EGLNativeWindowType)gs, NULL);
    if (surf == EGL_NO_SURFACE) { printf("FAIL eglCreateWindowSurface (0x%x)\n", eglGetError()); return 1; }
    printf("egl window surface over gbm: ok  <-- the piece vigil is missing\n");

    if (!eglMakeCurrent(dpy, surf, surf, ctx)) { printf("FAIL eglMakeCurrent\n"); return 1; }
    printf("GL_RENDERER: %s\n", glGetString(GL_RENDERER));

    glClearColor(0.1f, 0.4f, 0.7f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    // Read BEFORE the swap: afterwards the back buffer's contents are
    // undefined, and reading there reports black no matter what was drawn.
    unsigned char px[4] = {0};
    glReadPixels(320, 240, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    printf("center pixel (expect ~25,102,178): %u,%u,%u\n", px[0], px[1], px[2]);
    if (px[0] < 15 || px[1] < 90 || px[2] < 160) { printf("FAIL nothing was drawn\n"); return 1; }
    if (!eglSwapBuffers(dpy, surf)) { printf("FAIL eglSwapBuffers\n"); return 1; }

    struct gbm_bo *bo = gbm_surface_lock_front_buffer(gs);
    if (!bo) { printf("FAIL lock_front_buffer\n"); return 1; }
    printf("front buffer locked: %ux%u stride=%u  (this is what gets a DRM fb)\n",
           gbm_bo_get_width(bo), gbm_bo_get_height(bo), gbm_bo_get_stride(bo));

    gbm_surface_release_buffer(gs, bo);
    printf("PROBE OK\n");
    return 0;
}
