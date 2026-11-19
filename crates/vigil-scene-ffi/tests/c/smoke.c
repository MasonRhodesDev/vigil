#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vigil_scene.h"

static int fail(const char *what, int got) {
    fprintf(stderr, "%s: %d\n", what, got);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 3)
        return fail("usage: smoke LOGO.svg OUT", argc);
    FILE *f = fopen(argv[1], "rb");
    if (!f)
        return fail("open logo", 0);
    static unsigned char svg[1 << 16];
    size_t svg_len = fread(svg, 1, sizeof svg, f);
    fclose(f);

    VigilSceneRequest req;
    memset(&req, 0, sizeof req);
    req.svg = svg;
    req.svg_len = svg_len;
    req.mode_width = 3840;
    req.mode_height = 2160;
    req.transform = 1;
    req.layer = VIGIL_SCENE_LAYER_LOGO;
    req.deadline_ms = VIGIL_SCENE_MAX_DEADLINE_MS;

    VigilScenePlacement at;
    int rc = vigil_scene_render(NULL, NULL, 0, &at);
    if (rc != VIGIL_SCENE_ERR_ARGUMENT)
        return fail("null request", rc);
    rc = vigil_scene_render(&req, NULL, 0, &at);
    if (rc != VIGIL_SCENE_OK)
        return fail("size query", rc);
    size_t need = at.stride * at.height;
    unsigned char *px = malloc(need);
    rc = vigil_scene_render(&req, px, need - 1, &at);
    if (rc != VIGIL_SCENE_ERR_BUFFER)
        return fail("short buffer", rc);
    rc = vigil_scene_render(&req, px, need, &at);
    if (rc != VIGIL_SCENE_OK)
        return fail("render", rc);

    FILE *out = fopen(argv[2], "wb");
    if (!out)
        return fail("open out", 0);
    fwrite(px, 1, need, out);
    fclose(out);
    printf("%lld %lld %u %u %zu\n", (long long)at.x, (long long)at.y, at.width, at.height, at.stride);
    free(px);
    return 0;
}
