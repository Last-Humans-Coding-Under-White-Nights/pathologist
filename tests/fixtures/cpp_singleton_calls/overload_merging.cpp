#include "sptr.h"

class Decoy2 {
public:
    void Run() {}
};

// An in-class definition merging into a same-arity prototype (finding R2-1).
class MixSvc {
public:
    void Run() {}
};
template <typename T> class Mixed {
public:
    static Decoy2 *Get(const char *key);
    static T *Get(int key) { return nullptr; }
};
void mixed_get() { Mixed<MixSvc>::Get(lookup_key())->Run(); }

// A plain class in a unit that uses templates keeps its first return (R2-2).
class Bar2 {
public:
    void Foo() {}
};
struct Plain {
    Bar2 *Get(long key);
    auto Get(int key);
};
void plain_get(Plain *p) { p->Get(1L)->Foo(); }

// A template template parameter is no wrapper class (R2-3).
class Holder2 {
public:
    void Run() {}
};
class TtSvc {
public:
    void Run() {}
};
template <template <class> class Holder2, class T> class Tt {
public:
    static Holder2<T> Get();
};
void tt_get() { Tt<sptr, TtSvc>::Get()->Run(); }

// `Wrap<T>::GetInstance()` inside a template names no class (R2-4).
// A real class spelled like `Wrap`'s base parameter.
class U {
public:
    static Decoy2 *GetInstance();
};
template <class U> class Wrap : public U {};
template <class T> void wrap_dependent() { Wrap<T>::GetInstance()->Run(); }
