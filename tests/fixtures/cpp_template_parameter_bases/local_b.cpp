#include "local.h"

namespace {
template <class T> struct Impl : Pair<T, IExtra> {};
}
