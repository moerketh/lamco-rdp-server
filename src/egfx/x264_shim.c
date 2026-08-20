#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <x264.h>

typedef struct {
    void *library;
    x264_t *encoder;
    x264_picture_t input;
    x264_picture_t output;
} lamco_x264_encoder;

typedef x264_t *(*x264_encoder_open_fn)(x264_param_t *);
typedef int (*x264_encoder_encode_fn)(x264_t *, x264_nal_t **, int *, x264_picture_t *, x264_picture_t *);
typedef void (*x264_encoder_close_fn)(x264_t *);
typedef void (*x264_picture_init_fn)(x264_picture_t *);
typedef int (*x264_param_default_preset_fn)(x264_param_t *, const char *, const char *);
typedef int (*x264_param_parse_fn)(x264_param_t *, const char *, const char *);
typedef void (*x264_param_cleanup_fn)(x264_param_t *);

static void *load_symbol(void *library, const char *name) {
    return dlsym(library, name);
}

void *lamco_x264_create(uint32_t width, uint32_t height, uint32_t fps,
                        uint32_t qp_min, uint32_t qp_max, uint32_t threads) {
    const char *names[] = {"libx264.so.164", "libx264.so.148", "libx264.so"};
    void *library = NULL;
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); ++i) {
        library = dlopen(names[i], RTLD_NOW | RTLD_LOCAL);
        if (library) break;
    }
    if (!library) return NULL;

    x264_param_default_preset_fn default_preset =
        (x264_param_default_preset_fn)load_symbol(library, "x264_param_default_preset");
    x264_param_parse_fn parse =
        (x264_param_parse_fn)load_symbol(library, "x264_param_parse");
    x264_param_cleanup_fn cleanup =
        (x264_param_cleanup_fn)load_symbol(library, "x264_param_cleanup");
    x264_encoder_open_fn open =
        (x264_encoder_open_fn)load_symbol(library, "x264_encoder_open_164");
    if (!open) open = (x264_encoder_open_fn)load_symbol(library, "x264_encoder_open_148");
    x264_picture_init_fn picture_init =
        (x264_picture_init_fn)load_symbol(library, "x264_picture_init");
    if (!default_preset || !parse || !cleanup || !open || !picture_init) {
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
    param.i_threads = (int)threads;
    param.i_keyint_max = 1000;
    param.i_keyint_min = 1000;
    param.i_scenecut_threshold = 0;
    param.i_bframe = 0;
    param.b_annexb = 1;
    param.b_repeat_headers = 1;
    param.b_aud = 0;
    param.b_intra_refresh = 0;
    param.rc.i_rc_method = X264_RC_CRF;
    param.rc.f_rf_constant = (float)qp_min;

    lamco_x264_encoder *result = calloc(1, sizeof(*result));
    if (!result) {
        cleanup(&param);
        dlclose(library);
        return NULL;
    }
    result->library = library;
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

int lamco_x264_encode(void *opaque, const uint8_t *y, const uint8_t *u,
                      const uint8_t *v, int y_stride, int uv_stride,
                      int width, int height, int64_t pts, int force_idr,
                      uint8_t **output, int *output_size, int *is_keyframe) {
    lamco_x264_encoder *encoder = (lamco_x264_encoder *)opaque;
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

    x264_encoder_encode_fn encode =
        (x264_encoder_encode_fn)load_symbol(encoder->library, "x264_encoder_encode");
    if (!encode) return -1;

    x264_nal_t *nals = NULL;
    int nal_count = 0;
    int result = encode(encoder->encoder, &nals, &nal_count, picture, &encoder->output);
    if (result <= 0 || nal_count <= 0 || !nals) return result;

    uint8_t *data = malloc((size_t)result);
    if (!data) return -1;
    int offset = 0;
    int keyframe = 0;
    for (int i = 0; i < nal_count; ++i) {
        if (nals[i].i_type == NAL_SLICE_IDR) keyframe = 1;
        if (nals[i].i_payload > 0) {
            memcpy(data + offset, nals[i].p_payload, (size_t)nals[i].i_payload);
            offset += nals[i].i_payload;
        }
    }
    *output = data;
    *output_size = offset;
    *is_keyframe = keyframe || picture->b_keyframe;
    return offset;
}

void lamco_x264_free(void *data) {
    free(data);
}

void lamco_x264_destroy(void *opaque) {
    lamco_x264_encoder *encoder = (lamco_x264_encoder *)opaque;
    if (!encoder) return;
    x264_encoder_close_fn close =
        (x264_encoder_close_fn)load_symbol(encoder->library, "x264_encoder_close");
    if (close && encoder->encoder) close(encoder->encoder);
    if (encoder->library) dlclose(encoder->library);
    free(encoder);
}
