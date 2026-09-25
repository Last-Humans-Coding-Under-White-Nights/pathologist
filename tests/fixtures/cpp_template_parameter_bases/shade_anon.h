#include "local.h"

// A file-local `Shade` in a header: only this file's classes see it.
namespace {
template <class T> struct Shade : Pair<T, IExtra> {};
}
