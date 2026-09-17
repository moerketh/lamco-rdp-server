/* Multi-ABI x264 dispatcher.
 *
 * x264 has no stable ABI across releases: x264_param_t (the struct every
 * encoder open call passes BY LAYOUT) changes between builds, so the
 * library versions its open symbol (x264_encoder_open_<build>) and its
 * soname (libx264.so.<build>). One prebuilt deb therefore cannot link
 * against every distro's x264 the way it can against, say,
 * libopenh264.so.8 (stable soname for years).
 *
 * The dispatcher holds one per-build instance (x264_164.c / x264_165.c,
 * each compiled against ITS OWN vendored x264.h — exact-ABI per path) and
 * routes the public unsuffixed API to whichever build's library is
 * loadable at runtime. Preference order: newest first. Selection is
 * sticky: an encoder opened by one instance stays bound to it for its
 * whole lifetime.
 *
 * Adding a build: vendor headers under src/egfx/x264/<build>/, add the
 * one-line TU, register it in build.rs and the table below. */

#include <stddef.h>
#include <stdint.h>

typedef struct {
    int build;
    int (*probe)(void);
    void *(*create)(uint32_t, uint32_t, uint32_t, uint32_t, uint32_t, uint32_t,
                    uint32_t);
    int (*encode)(void *, const uint8_t *, const uint8_t *, const uint8_t *, int,
                  int, int, int, int64_t, int, uint8_t **, int *, int *);
    void (*free)(void *);
    void (*destroy)(void *);
} lamco_x264_backend;

/* Declare the per-build symbol sets (defined in x264_<build>.c). */
#define LAMCO_DECL_(b)                                                    \
    extern int lamco_x264_##b##_probe(void);                              \
    extern void *lamco_x264_##b##_create(uint32_t, uint32_t, uint32_t,    \
                                         uint32_t, uint32_t, uint32_t,    \
                                         uint32_t);                       \
    extern int lamco_x264_##b##_encode(void *, const uint8_t *,           \
                                       const uint8_t *, const uint8_t *,  \
                                       int, int, int, int, int64_t, int,  \
                                       uint8_t **, int *, int *);         \
    extern void lamco_x264_##b##_free(void *);                            \
    extern void lamco_x264_##b##_destroy(void *);

LAMCO_DECL_(164)
LAMCO_DECL_(165)

static const lamco_x264_backend lamco_x264_backends[] = {
    {165, lamco_x264_165_probe, lamco_x264_165_create, lamco_x264_165_encode,
     lamco_x264_165_free, lamco_x264_165_destroy},
    {164, lamco_x264_164_probe, lamco_x264_164_create, lamco_x264_164_encode,
     lamco_x264_164_free, lamco_x264_164_destroy},
};

/* Selected once at first use (the selection ladder calls probe() before
 * any encoder exists); sticky so a mid-session library change cannot
 * split an encoder across instances. */
static const lamco_x264_backend *lamco_x264_selected = 0;
static int lamco_x264_selection_done = 0;

static const lamco_x264_backend *select_backend(void) {
    if (!lamco_x264_selection_done) {
        for (size_t i = 0;
             i < sizeof(lamco_x264_backends) / sizeof(lamco_x264_backends[0]); ++i) {
            if (lamco_x264_backends[i].probe()) {
                lamco_x264_selected = &lamco_x264_backends[i];
                break;
            }
        }
        lamco_x264_selection_done = 1;
    }
    return lamco_x264_selected;
}

int lamco_x264_probe(void) {
    return select_backend() != 0;
}

int lamco_x264_active_build(void) {
    const lamco_x264_backend *b = select_backend();
    return b ? b->build : 0;
}

void *lamco_x264_create(uint32_t width, uint32_t height, uint32_t fps,
                        uint32_t qp_min, uint32_t qp_max, uint32_t threads,
                        uint32_t fullrange) {
    const lamco_x264_backend *b = select_backend();
    if (!b) return 0;
    return b->create(width, height, fps, qp_min, qp_max, threads, fullrange);
}

int lamco_x264_encode(void *opaque, const uint8_t *y, const uint8_t *u,
                      const uint8_t *v, int y_stride, int uv_stride, int width,
                      int height, int64_t pts, int force_idr, uint8_t **output,
                      int *output_size, int *is_keyframe) {
    const lamco_x264_backend *b = select_backend();
    if (!b) return -1;
    return b->encode(opaque, y, u, v, y_stride, uv_stride, width, height, pts,
                     force_idr, output, output_size, is_keyframe);
}

void lamco_x264_free(void *data) {
    /* free() is build-independent: the NAL buffer is plain malloc'd by the
     * same toolchain regardless of instance. */
    lamco_x264_164_free(data);
}

void lamco_x264_destroy(void *opaque) {
    /* Route destroy back through the selected (sticky) backend so the
     * encoder is closed and dlclosed by the instance that created it. */
    const lamco_x264_backend *b = select_backend();
    if (!b) return;
    b->destroy(opaque);
}
