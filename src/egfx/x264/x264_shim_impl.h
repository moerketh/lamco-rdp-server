/* Multi-ABI x264 shim: per-build implementation header.
 *
 * Included by x264_164.c / x264_165.c AFTER their vendored x264.h, so every
 * translation unit is compiled against the header matching the library
 * build it calls — the exact-ABI property the shim has always enforced,
 * now per build instead of for exactly one build.
 *
 * Each TU defines LAMCO_X264_BUILD (its build number) before including
 * this file; all extern symbols are suffixed with that number
 * (lamco_x264_164_create, lamco_x264_165_create, ...). The dispatcher in
 * x264_shim.c routes the unsuffixed public API to whichever build's
 * library is loadable at runtime.
 *
 * Adding a new x264 release: vendor its x264.h + x264_config.h under
 * src/egfx/x264/<build>/, add a one-line TU, register it in build.rs and
 * the dispatcher's table. Nothing else changes. */

#define LAMCO_PASTE2_(a, b) a##b
#define LAMCO_PASTE_(a, b) LAMCO_PASTE2_(a, b)
#define LAMCO_X264_FN_(name) \
    LAMCO_PASTE_(lamco_x264_, LAMCO_PASTE_(LAMCO_X264_BUILD, _##name))

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>

/* Compile-time ABI gate: x264 versions its encoder-open symbol precisely
 * because x264_param_t changes layout between builds. Stringify the
 * X264_BUILD the VENDORED HEADER declares and require exactly that symbol
 * at runtime — a library exporting a different version must fail cleanly
 * here, not be called through a mismatched struct layout (which is UB:
 * garbage parameters at best, heap corruption at worst). */
#define LAMCO_STR2_(x) #x
#define LAMCO_STR_(x) LAMCO_STR2_(x)
#define LAMCO_X264_OPEN_SYMBOL_ \
    "x264_encoder_open_" LAMCO_STR_(X264_BUILD)
#define LAMCO_X264_SONAME_ "libx264.so." LAMCO_STR_(X264_BUILD)

typedef x264_t *(*x264_encoder_open_fn)(x264_param_t *);
typedef int (*x264_encoder_encode_fn)(x264_t *, x264_nal_t **, int *,
                                      x264_picture_t *, x264_picture_t *);
typedef void (*x264_encoder_close_fn)(x264_t *);
typedef void (*x264_picture_init_fn)(x264_picture_t *);
typedef int (*x264_param_default_preset_fn)(x264_param_t *, const char *,
                                            const char *);
typedef int (*x264_param_parse_fn)(x264_param_t *, const char *, const char *);
typedef int (*x264_param_apply_profile_fn)(x264_param_t *, const char *);
typedef void (*x264_param_cleanup_fn)(x264_param_t *);

typedef struct {
    void *library;
    x264_t *encoder;
    x264_picture_t input;
    x264_picture_t output;
    /* Cached hot-path symbols, resolved once at create: encode runs per
     * frame and close per teardown; dlsym on each was pure overhead (and
     * the only per-call lookup left in this shim). */
    x264_encoder_encode_fn encode;
    x264_encoder_close_fn close_fn;
} lamco_x264_encoder_;

static void *load_symbol_(void *library, const char *name) {
    return dlsym(library, name);
}

/* Load this build's exact soname (then the unversioned fallback, which
 * only exists with libx264-dev installed). NULL when absent. */
static void *load_library_(void) {
    const char *names[] = {LAMCO_X264_SONAME_, "libx264.so"};
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); ++i) {
        void *library = dlopen(names[i], RTLD_NOW | RTLD_LOCAL);
        if (library) return library;
    }
    return NULL;
}

void *LAMCO_X264_FN_(create)(uint32_t width, uint32_t height, uint32_t fps,
                             uint32_t qp_min, uint32_t qp_max, uint32_t threads,
                             uint32_t fullrange) {
    void *library = load_library_();
    if (!library) return NULL;

    x264_param_default_preset_fn default_preset =
        (x264_param_default_preset_fn)load_symbol_(library, "x264_param_default_preset");
    x264_param_apply_profile_fn apply_profile =
        (x264_param_apply_profile_fn)load_symbol_(library, "x264_param_apply_profile");
    x264_param_cleanup_fn cleanup =
        (x264_param_cleanup_fn)load_symbol_(library, "x264_param_cleanup");
    /* Exact-ABI open symbol (see LAMCO_X264_OPEN_SYMBOL_ above). No
     * cross-version fallback: a version mismatch must fail loudly, not
     * corrupt memory through a wrong-layout param struct. */
    x264_encoder_open_fn open =
        (x264_encoder_open_fn)load_symbol_(library, LAMCO_X264_OPEN_SYMBOL_);
    x264_picture_init_fn picture_init =
        (x264_picture_init_fn)load_symbol_(library, "x264_picture_init");
    x264_encoder_encode_fn encode =
        (x264_encoder_encode_fn)load_symbol_(library, "x264_encoder_encode");
    x264_encoder_close_fn close_fn =
        (x264_encoder_close_fn)load_symbol_(library, "x264_encoder_close");
    if (!default_preset || !apply_profile || !cleanup || !open || !picture_init ||
        !encode || !close_fn) {
        dlclose(library);
        return NULL;
    }

    x264_param_t param;
    if (default_preset(&param, "ultrafast", "zerolatency") != 0) {
        dlclose(library);
        return NULL;
    }
    param.i_width = (int)width;
    param.i_height = (int)height;
    param.i_csp = X264_CSP_I420;
    param.i_fps_num = fps ? fps : 60;
    param.i_fps_den = 1;
    param.rc.i_qp_min = (int)qp_min;
    param.rc.i_qp_max = (int)qp_max;
    /* Threading: zerolatency pins i_threads=1 (no frame delay). Re-raising
     * i_threads WITHOUT sliced threading would switch x264 to FRAME
     * threading: the first N encoded frames (including the connect-time IDR
     * on a static desktop, which may be the ONLY frame for minutes) sit in
     * the thread pipeline and x264_encoder_encode returns 0 output — the
     * client gets an EGFX surface and never a single video frame. Sliced
     * threading keeps the parallelism with per-frame synchronous output. */
    if (threads > 1) {
        param.i_threads = (int)threads;
        param.b_sliced_threads = 1;
    } else {
        param.i_threads = 1;
        param.b_sliced_threads = 0;
    }
    param.i_keyint_max = 1000;
    param.i_keyint_min = 1000;
    param.i_scenecut_threshold = 0;
    param.i_bframe = 0;
    param.b_annexb = 1;
    param.b_repeat_headers = 1;
    param.b_aud = 0;
    param.b_intra_refresh = 0;
    param.rc.i_rc_method = X264_RC_CRF;
    /* Colorimetry VUI: signal BT.601 with the requested range. mstsc does
     * not perform limited->full expansion (limited black Y16 renders as
     * RGB 16 = grey), so full-range encoding + flag is used to keep black
     * rendering as black. Values: colmatrix 6=BT.601 (SMPTE170M),
     * colorprim 6=SMPTE170M, transfer 1=BT.709 (standard for computer
     * graphics per sRGB). */
    param.vui.i_colmatrix = 6;   /* X264_VUI_COLORSPEC_SMPTE170M */
    param.vui.i_colorprim = 6;   /* SMPTE170M */
    param.vui.i_transfer = 1;    /* BT.709 */
    param.vui.b_fullrange = fullrange ? 1 : 0;
    /* CRF 0 is lossless and rejected by the 4:2:0 profiles required for
     * MS-RDPEGFX AVC420. Screen content is visually lossless around CRF 15;
     * OpenH264-style qp_min values (0-10) must not map to CRF 1, which is
     * near-lossless and makes the encoder much slower for no perceptible
     * gain on text/UI. */
    param.rc.f_rf_constant = qp_min < 15 ? 15.0f : (qp_min > 30 ? 30.0f : (float)qp_min);
    if (apply_profile(&param, "main") != 0) {
        cleanup(&param);
        dlclose(library);
        return NULL;
    }

    lamco_x264_encoder_ *result = calloc(1, sizeof(*result));
    if (!result) {
        cleanup(&param);
        dlclose(library);
        return NULL;
    }
    result->library = library;
    result->encode = encode;
    result->close_fn = close_fn;
    result->encoder = open(&param);
    cleanup(&param);
    if (!result->encoder) {
        dlclose(library);
        free(result);
        return NULL;
    }
    picture_init(&result->input);
    picture_init(&result->output);
    return result;
}

int LAMCO_X264_FN_(encode)(void *opaque, const uint8_t *y, const uint8_t *u,
                           const uint8_t *v, int y_stride, int uv_stride,
                           int width, int height, int64_t pts, int force_idr,
                           uint8_t **output, int *output_size,
                           int *is_keyframe) {
    lamco_x264_encoder_ *encoder = (lamco_x264_encoder_ *)opaque;
    (void)width;
    (void)height;
    if (!encoder || !output || !output_size || !is_keyframe) return -1;

    x264_picture_t *picture = &encoder->input;
    memset(picture, 0, sizeof(*picture));
    picture->i_type = force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
    picture->i_pts = pts;
    picture->img.i_csp = X264_CSP_I420;
    picture->img.i_plane = 3;
    picture->img.i_stride[0] = y_stride;
    picture->img.i_stride[1] = uv_stride;
    picture->img.i_stride[2] = uv_stride;
    picture->img.plane[0] = (uint8_t *)y;
    picture->img.plane[1] = (uint8_t *)u;
    picture->img.plane[2] = (uint8_t *)v;

    x264_nal_t *nals = NULL;
    int nal_count = 0;
    int result =
        encoder->encode(encoder->encoder, &nals, &nal_count, picture, &encoder->output);
    if (result <= 0 || nal_count <= 0 || !nals) return result;

    uint8_t *data = malloc((size_t)result);
    if (!data) return -1;
    int offset = 0;
    int keyframe = 0;
    for (int i = 0; i < nal_count; ++i) {
        if (nals[i].i_type == NAL_SLICE_IDR) keyframe = 1;
        if (nals[i].i_payload > 0) {
            /* x264's contract is sum(i_payload) == return value; the
             * running bound check turns any contract violation (e.g. a
             * subtly ABI-mismatched library) into a truncated copy instead
             * of a heap overflow. */
            if (offset + nals[i].i_payload > result) break;
            memcpy(data + offset, nals[i].p_payload, (size_t)nals[i].i_payload);
            offset += nals[i].i_payload;
        }
    }
    *output = data;
    *output_size = offset;
    *is_keyframe = keyframe || picture->b_keyframe;
    return offset;
}

void LAMCO_X264_FN_(free)(void *data) {
    free(data);
}

void LAMCO_X264_FN_(destroy)(void *opaque) {
    lamco_x264_encoder_ *encoder = (lamco_x264_encoder_ *)opaque;
    if (!encoder) return;
    if (encoder->encoder && encoder->close_fn) {
        encoder->close_fn(encoder->encoder);
    }
    if (encoder->library) {
        dlclose(encoder->library);
    }
    free(encoder);
}

/* Selection-time probe for this build: verifies the exact-ABI soname AND
 * open symbol are loadable on THIS system, without building an encoder. */
int LAMCO_X264_FN_(probe)(void) {
    void *library = load_library_();
    if (!library) return 0;
    void *open = load_symbol_(library, LAMCO_X264_OPEN_SYMBOL_);
    if (!open) {
        dlclose(library);
        return 0;
    }
    dlclose(library);
    return 1;
}
