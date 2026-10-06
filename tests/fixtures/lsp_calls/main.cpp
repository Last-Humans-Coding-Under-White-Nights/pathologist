#include "requests.h"

void alpha() {}
void beta() {}
void ambiguous_target() {}
void prototype_only(int value);
static void helper() { alpha(); }
void cycle_b();
void cycle_a() { cycle_b(); }
void cycle_b() { cycle_a(); }
void recursive() { recursive(); }
void overloaded(int value) { alpha(); }
void overloaded(double value) { beta(); }
void unresolved(void (*unknown)()) { unknown(); }
void caller() {
    alpha(); alpha();
    /*é😀*/ beta();
    REQUEST_ALPHA();
    prototype_only(1);
    no_source_external();
    helper();
    overloaded(1);
    overloaded(1.0);
}
