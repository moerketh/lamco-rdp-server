/* Build-165 instance: compiled against the vendored 165 headers only.
 * x264.h requires stdint.h/inttypes.h to be included first (it warns
 * and misdeclares types otherwise). */
#include <stdint.h>
#include "x264/165/x264.h"
#define LAMCO_X264_BUILD 165
#include "x264/x264_shim_impl.h"
