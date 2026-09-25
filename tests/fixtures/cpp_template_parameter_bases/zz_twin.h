#include "local.h"

namespace {
template <class T> struct Twin : T {};
struct TwinOne : Twin<ILocal> {};
}
