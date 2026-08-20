#include <stdio.h>
#include <stdint.h>
#include <stddef.h>
#include <x264.h>
int main() {
    printf("sizeof(x264_nal_t)=%zu\n", sizeof(x264_nal_t));
    printf("nal.i_ref_idc=%zu\n", offsetof(x264_nal_t, i_ref_idc));
    printf("nal.i_type=%zu\n", offsetof(x264_nal_t, i_type));
    printf("nal.b_long_startcode=%zu\n", offsetof(x264_nal_t, b_long_startcode));
    printf("nal.i_first_mb=%zu\n", offsetof(x264_nal_t, i_first_mb));
    printf("nal.i_last_mb=%zu\n", offsetof(x264_nal_t, i_last_mb));
    printf("nal.i_payload=%zu\n", offsetof(x264_nal_t, i_payload));
    printf("nal.p_payload=%zu\n", offsetof(x264_nal_t, p_payload));
    printf("nal.i_padding=%zu\n", offsetof(x264_nal_t, i_padding));
    return 0;
}