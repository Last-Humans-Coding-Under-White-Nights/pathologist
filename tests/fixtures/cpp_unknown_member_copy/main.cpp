#include "owners.hpp"
struct UnknownCopyOwner {
    External value;
    UnknownCopyOwner(const External &other) : value(other) {}
};
