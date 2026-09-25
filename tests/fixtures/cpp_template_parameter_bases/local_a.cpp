#include "local.h"

// A file-local template: another unit's `Impl` is another template.
namespace {
template <class T> struct Impl : T {};
struct LocalA : Impl<ILocal> {};
}

void UseLocalA() { LocalA a; (void)a; }
