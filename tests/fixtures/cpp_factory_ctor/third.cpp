// A third unit using the shared header's factory (#192).
#include "factory.h"

void third_unique() { auto b = std::make_unique<Foo>(2, Handler); }
void third_sptr() { auto s = OHOS::sptr<Foo>::MakeSptr(4, Handler); }
void third_header() { from_header(); }
