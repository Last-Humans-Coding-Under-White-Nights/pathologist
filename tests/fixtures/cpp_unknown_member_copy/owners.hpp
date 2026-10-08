#include "external.hpp"
struct CachedUnknownCopyOwner {
    External value;
    CachedUnknownCopyOwner(const External &other) : value(other) {}
};
