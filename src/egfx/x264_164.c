/* Build-164 instance: compiled against the vendored 164 headers only.
 * x264.h requires stdint.h/inttypes.h to be included first (it warns
 * and misdeclares types otherwise). */
#include <stdint.h>
#include "x264/164/x264.h"
#define LAMCO_X264_BUILD 164
#include "x264/x264_shim_impl.h"
