#include "detached.hpp"

void OnDetachedLater() {}
void OnDetachedShared() {}

void UseDetached(Detached &d) {
    d.Later(OnDetachedLater);
    Detached::Shared(OnDetachedShared);
}
