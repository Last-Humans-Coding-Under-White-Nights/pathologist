#include <config.h>
void target(void) {}
void entry(void) {
#if HEADER_OK && FORCED_OK && MODE == 3 && !defined(REMOVED)
    target();
#endif
}
