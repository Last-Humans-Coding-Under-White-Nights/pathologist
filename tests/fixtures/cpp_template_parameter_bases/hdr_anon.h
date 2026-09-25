#include "local.h"

// A file-local template in a header: every unit including it sees it.
namespace {
template <class T> struct HdrLocal : T {};
}
