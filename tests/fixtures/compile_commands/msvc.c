#include <config.h>
#if HEADER_OK && FORCED_OK && VALUE == 2 && !defined(OLD)
namespace N { void target() {} void entry() { target(); } }
#endif
