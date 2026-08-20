#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <x264.h>

void bgra_to_i420(const uint8_t *bgra, int width, int height,
                  uint8_t *y, uint8_t *u, uint8_t *v) {
    int y_stride = width;
    int uv_stride = width / 2;
    for (int j = 0; j < height; j++) {
        for (int i = 0; i < width; i++) {
            int idx = (j * width + i) * 4;
            int b = bgra[idx], g = bgra[idx+1], r = bgra[idx+2];
            y[j * y_stride + i] = (uint8_t)((66*r + 129*g + 25*b + 128) >> 8) + 16;
        }
    }
    for (int j = 0; j < height/2; j++) {
        for (int i = 0; i < width/2; i++) {
            int idx = ((j*2) * width + (i*2)) * 4;
            int b = bgra[idx], g = bgra[idx+1], r = bgra[idx+2];
            u[j * uv_stride + i] = (uint8_t)((-38*r - 74*g + 112*b + 128) >> 8) + 128;
            v[j * uv_stride + i] = (uint8_t)((112*r - 94*g - 18*b + 128) >> 8) + 128;
        }
    }
}

int main() {
    x264_param_t param;
    x264_param_default_preset(&param, "ultrafast", "zerolatency");
    
    int width = 1920, height = 1088;
    param.i_width = width;
    param.i_height = height;
    param.i_csp = X264_CSP_I420;
    param.i_threads = 0;
    param.i_fps_num = 60;
    param.i_fps_den = 1;
    param.rc.i_qp_min = 1;
    param.i_keyint_max = 1000;
    param.i_bframe = 0;
    param.b_annexb = 1;
    param.i_log_level = X264_LOG_INFO;
    
    printf("param.i_csp = 0x%x (I420=0x2)\n", param.i_csp);
    
    x264_t *encoder = x264_encoder_open(&param);
    if (!encoder) { printf("ERROR: open failed\n"); return 1; }
    printf("Encoder opened\n");
    
    int bgra_size = width * height * 4;
    uint8_t *bgra = malloc(bgra_size);
    for (int i = 0; i < bgra_size; i += 4) {
        bgra[i] = 0x80; bgra[i+1] = 0x60; bgra[i+2] = 0x40; bgra[i+3] = 0xFF;
    }
    
    int y_size = width * height;
    int uv_size = (width/2) * (height/2);
    uint8_t *y = malloc(y_size);
    uint8_t *u = malloc(uv_size);
    uint8_t *v = malloc(uv_size);
    bgra_to_i420(bgra, width, height, y, u, v);
    
    x264_picture_t pic_in, pic_out;
    x264_picture_init(&pic_in);
    pic_in.img.i_csp = X264_CSP_I420;
    pic_in.img.i_plane = 3;
    pic_in.img.i_stride[0] = width;
    pic_in.img.i_stride[1] = width/2;
    pic_in.img.i_stride[2] = width/2;
    pic_in.img.plane[0] = y;
    pic_in.img.plane[1] = u;
    pic_in.img.plane[2] = v;
    pic_in.i_pts = 0;
    pic_in.i_type = X264_TYPE_IDR;
    
    x264_nal_t *nals = NULL;
    int i_nal = 0;
    int ret = x264_encoder_encode(encoder, &nals, &i_nal, &pic_in, &pic_out);
    printf("ret=%d, i_nal=%d\n", ret, i_nal);
    if (i_nal > 0) {
        int total = 0;
        for (int i = 0; i < i_nal; i++) {
            printf("  NAL %d: type=%d, payload=%d\n", i, nals[i].i_type, nals[i].i_payload);
            total += nals[i].i_payload;
        }
        printf("Total: %d bytes\n", total);
        printf("First NAL: ");
        for (int j = 0; j < 32 && j < nals[0].i_payload; j++) printf("%02x ", nals[0].p_payload[j]);
        printf("\n");
    }
    
    x264_encoder_close(encoder);
    free(bgra); free(y); free(u); free(v);
    return 0;
}