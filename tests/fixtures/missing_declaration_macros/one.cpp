#include "api.h"
int helper(int value) { return value; }
namespace ffi {
FFI_EXPORT int Defined(int value) { return helper(value); }
FFI_EXPORT extern "C" const char *CBefore(int *value) { return nullptr; }
extern "C" FFI_EXPORT bool CAfter(long value) { return true; }
extern "C" {
FFI_EXPORT int CBody(double value) { return helper(1); }
}
}
