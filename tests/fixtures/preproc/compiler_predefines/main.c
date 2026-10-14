#include "section.h"
#if defined(__clang__)
void clang_identity(void) {}
#elif defined(__GNUC__)
void gnu_identity(void) {}
#elif defined(__ICCARM__)
void iar_identity(void) {}
#else
void absent_identity(void) {}
#endif
#if defined(__MINGW32__) || defined(__MINGW64__)
void mingw_target(void) {}
#endif
#if defined(__aarch64__)
void arm_target(void) {}
#elif defined(__x86_64__)
void x64_target(void) {}
#endif
/* HiView checks MSVC before GNU. */
#if defined(_MSC_VER)
void hiview_msvc(void) {}
#elif defined(__GNUC__)
void hiview_gnu(void) {}
#else
void hiview_unknown(void) {}
#endif
#ifdef __BYTE_ORDER
#error "system-header endianness must not be synthesized"
#endif
#ifdef __USER__
#error "build identity must not be synthesized"
#endif
