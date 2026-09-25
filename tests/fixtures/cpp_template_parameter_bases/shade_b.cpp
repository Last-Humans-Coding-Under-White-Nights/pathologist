#include "local.h"

// Another unit's file-local `Shade`: another template again.
namespace {
template <class T> struct Shade : Pair<T, IExtra> {};
}
