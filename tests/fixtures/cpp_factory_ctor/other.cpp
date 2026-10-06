// A second unit using the shared header's factory (#192).
#include "factory.h"

void other_shared() { auto a = std::make_shared<Foo>(1, Handler); }
void other_header() { from_header(); }
