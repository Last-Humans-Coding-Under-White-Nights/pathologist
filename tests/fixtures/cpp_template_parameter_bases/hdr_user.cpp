#include "hdr_anon.h"

// Derives through the included header's file-local template.
struct HdrUser : HdrLocal<ILocal> {};
namespace {
struct HdrAnonUser : HdrLocal<IExtra> {};
}
