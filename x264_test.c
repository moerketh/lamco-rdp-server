#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <x264.h>

int main() {
    x264_param_t param;
    x264_param_default_preset(&param, "ultrafast", "zerolatency");
    
    int width = 1920, height = 1088;
    param.i_width = width;
    param.i_height = height;
    param.i_csp = X264_CSP_BGRA;
    param.i_threads = 0;
    param.i_fps_num = 60;
    param.i_fps_den = 1;
    param.rc.i_qp_min = 1;
    param.rc.i_qp_max = 10;
    param.i_keyint_max = 1000;
    param.i_bframe = 0;
    param.b_annexb = 1;
    param.i_log_level = X264_LOG_INFO;
    
    printf("x264 build: %d\n", X264_BUILD);
    printf("X264_CSP_BGRA = 0x%x\n", X264_CSP_BGRA);
    printf("param.i_csp = 0x%x\n", param.i_csp);
    printf("width=%d height=%d\n", width, height);
    
    x264_t *encoder = x264_encoder_open(&param);
    if (!encoder) {
        printf("ERROR: x264_encoder_open failed\n");
        return 1;
    }
    printf("Encoder opened successfully\n");
    
    // Create a test BGRA frame (solid color)
    int frame_size = width * height * 4;
    uint8_t *bgra = malloc(frame_size);
    // Fill with a non-trivial pattern
    for (int i = 0; i < frame_size; i += 4) {
        bgra[i] = 0x80;     // B
        bgra[i+1] = 0x60;   // G
        bgra[i+2] = 0x40;   // R
        bgra[i+3] = 0xFF;   // A
    }
    
    x264_picture_t pic_in, pic_out;
    x264_picture_init(&pic_in);
    pic_in.img.i_csp = X264_CSP_BGRA;
    pic_in.img.i_plane = 1;
    pic_in.img.i_stride[0] = width * 4;
    pic_in.img.plane[0] = bgra;
    pic_in.i_pts = 0;
    pic_in.i_type = X264_TYPE_IDR;
    
    x264_nal_t *nals = NULL;
    int i_nal = 0;
    
    printf("Encoding IDR frame...\n");
    int ret = x264_encoder_encode(encoder, &nals, &i_nal, &pic_in, &pic_out);
    printf("ret=%d, i_nal=%d\n", ret, i_nal);
    
    if (i_nal > 0 && nals != NULL) {
        int total_size = 0;
        for (int i = 0; i < i_nal; i++) {
            printf("  NAL %d: type=%d, ref_idc=%d, payload=%d bytes\n",
                   i, nals[i].i_type, nals[i].i_ref_idc, nals[i].i_payload);
            total_size += nals[i].i_payload;
        }
        printf("Total H.264 data: %d bytes\n", total_size);
        
        // Dump first 32 bytes of first NAL
        if (nals[0].i_payload >= 4) {
            printf("First NAL start: ");
            for (int j = 0; j < (nals[0].i_payload < 32 ? nals[0].i_payload : 32); j++) {
                printf("%02x ", nals[0].p_payload[j]);
            }
            printf("\n");
        }
    } else {
        printf("ERROR: No NALs produced!\n");
    }
    
    // Encode a P-frame
    pic_in.i_pts = 1;
    pic_in.i_type = X264_TYPE_AUTO;
    // Change some pixels
    for (int i = 0; i < frame_size; i += 4) {
        bgra[i] = 0xFF;
        bgra[i+1] = 0xAA;
        bgra[i+2] = 0x55;
    }
    
    ret = x264_encoder_encode(encoder, &nals, &i_nal, &pic_in, &pic_out);
    printf("\nP-frame: ret=%d, i_nal=%d\n", ret, i_nal);
    if (i_nal > 0) {
        int total_size = 0;
        for (int i = 0; i < i_nal; i++) {
            printf("  NAL %d: type=%d, payload=%d bytes\n", i, nals[i].i_type, nals[i].i_payload);
            total_size += nals[i].i_payload;
        }
        printf("Total P-frame data: %d bytes\n", total_size);
    }
    
    // Check delayed frames
    int delayed = x264_encoder_delayed_frames(encoder);
    printf("\nDelayed frames: %d\n", delayed);
    
    x264_encoder_close(encoder);
    free(bgra);
    printf("Done\n");
    return 0;
}