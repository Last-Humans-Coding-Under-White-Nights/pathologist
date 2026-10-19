#include "api.hpp"

__attribute__((visibility("default"))) extern "C" void *attribute_first() {
    return nullptr;
}

extern "C" __attribute__((visibility("default"))) void *linkage_first() {
    return attribute_first();
}
