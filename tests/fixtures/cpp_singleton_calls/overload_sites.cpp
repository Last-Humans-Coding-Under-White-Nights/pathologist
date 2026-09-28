#include "overload_sites.h"

// R10-2: a reference variable reads one pointer layer deeper.
class R10Foo {};
struct R10Taker {
    void Take(R10Foo f);
    void Take(R10Foo *f);
};
void r10_reference(R10Taker *t, const R10Foo &f) { t->Take(f); }

// R10-3: an unresolved enum parameter is no exact match for a literal.
struct R10Mode {
    void SetMode(Mode m);
    void SetMode(long v);
};
void r10_enum(R10Mode *m) { m->SetMode(5); }

// R10-5: unnamed and named pointer references are one type.
struct R10Get {
    void Get(R10Foo *&);
};
void R10Get::Get(R10Foo *&out) {}
