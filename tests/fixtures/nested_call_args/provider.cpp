#include "common.h"

void target_cross_tu(void) {}

Callback make_cross_tu(void) {
    return target_cross_tu;
}

void consume_cross_tu(Callback f) {
    f();
}
