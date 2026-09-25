#include "local.h"

// Two headers each define a file-local `Twin` and derive through it.
namespace {
template <class T> struct Twin : Pair<T, IExtra> {};
struct TwinPair : Twin<ILocal> {};
}
