#include "vendor/foo_util.h"
#ifdef HAS_FOO_UTIL
int UseFooFallback() { return FooUtil(); }
#endif
