// Objects built by smart-pointer factories reach their constructor as
// `new T(args)` does (#192, docs/ANALYSIS.md "Factory construction").
// <memory> is not in the tree: the factories stay external.
// factory.h is shared with other.cpp and third.cpp (several translation
// units, for the --jobs determinism test and header deduplication).
#include "factory.h"

Foo::Foo(int a, void (*h)()) { h(); }

// ---- Each factory form against `new Foo(...)` ----
void by_new() { auto c = new Foo(3, Handler); }
void by_shared() { auto a = std::make_shared<Foo>(1, Handler); }
void by_unique() { auto b = std::make_unique<Foo>(2, Handler); }
void by_global_shared() { auto a = ::std::make_shared<Foo>(1, Handler); }
void by_statement() { std::make_shared<Foo>(1, Handler); }
void by_sptr() { auto s = OHOS::sptr<Foo>::MakeSptr(4, Handler); }
// The allocator comes first; the constructor's arguments follow it.
void by_allocate(std::allocator<Foo> &al) { auto a = std::allocate_shared<Foo>(al, 1, Handler); }

namespace uses_std {
using namespace std;
void bare_shared() { auto a = make_shared<Foo>(1, Handler); }
} // namespace uses_std

namespace imports_unique {
using std::make_unique;
void bare_unique() { auto b = make_unique<Foo>(2, Handler); }
} // namespace imports_unique

namespace uses_ohos {
using namespace OHOS;
void bare_sptr() { auto s = sptr<Foo>::MakeSptr(4, Handler); }
} // namespace uses_ohos

// A wrapper whose MakeSptr is declared in the tree, as refbase.h has it.
namespace decl {
template <class T> class sptr {
public:
    template <class... Args> static sptr<T> MakeSptr(Args &&...args);
};
} // namespace decl
void by_declared_sptr() { auto s = decl::sptr<Foo>::MakeSptr(5, Handler); }

// ---- Overloads: the factory picks what `new` picks ----
struct Multi {
    Multi(int a);
    Multi(void (*h)());
    Multi(int a, int b);
};
Multi::Multi(int a) {}
Multi::Multi(void (*h)()) { h(); }
Multi::Multi(int a, int b) {}

void multi_new_int() { auto m = new Multi(1); }
void multi_shared_int() { auto m = std::make_shared<Multi>(1); }
void multi_new_fn() { auto m = new Multi(Other); }
void multi_unique_fn() { auto m = std::make_unique<Multi>(Other); }
void multi_new_two() { auto m = new Multi(1, 2); }
void multi_shared_two() { auto m = std::make_shared<Multi>(1, 2); }

// ---- No user-provided constructor: none is invented ----
struct Plain {
    int x;
};
struct Defaulted {
    Defaulted() = default;
};
void no_ctor() {
    auto p = std::make_shared<Plain>();
    auto d = std::make_unique<Defaulted>();
    auto s = OHOS::sptr<Plain>::MakeSptr();
}

// ---- A user-provided constructor beside an in-class defaulted one ----
// The class-level check passes, so the defaulted overload still ranks and
// can be chosen, exactly as for `new Mixed()`.
struct Mixed {
    Mixed() = default;
    Mixed(int a);
};
Mixed::Mixed(int a) {}
void mixed_new_none() { auto m = new Mixed(); }
void mixed_shared_none() { auto m = std::make_shared<Mixed>(); }
void mixed_new_int() { auto m = new Mixed(1); }
void mixed_shared_int() { auto m = std::make_shared<Mixed>(1); }

// ---- A class the call site cannot name ----
namespace hidden {
struct Secret {
    Secret(void (*h)());
};
Secret::Secret(void (*h)()) { h(); }
} // namespace hidden
void unnamed() {
    auto s = std::make_shared<Secret>(Handler);
    auto t = OHOS::sptr<Secret>::MakeSptr(Handler);
}

// ---- Not a recognised factory ----
namespace no_using {
void bare_none() { auto b = make_shared<Foo>(2, Handler); }
} // namespace no_using
void other_wrapper() { auto w = OHOS::wrap<Foo>::MakeSptr(4, Handler); }

// ---- A template-dependent argument, beside a real class of that name ----
struct T {
    T(void (*h)());
};
T::T(void (*h)()) { h(); }
template <class T> void make_any() {
    auto a = std::make_shared<T>(Handler);
    auto b = OHOS::sptr<T>::MakeSptr(Handler);
}

// ---- What a factory-built constructor stores reads back through the result ----
// One class per form, built by that factory alone: no `new` of the class
// fills the field it reads, so only the factory's constructor call can.
#define HOLDER(Name)                                                          \
    struct Name {                                                             \
        Name(void (*h)());                                                    \
        void (*cb)();                                                         \
    };                                                                        \
    Name::Name(void (*h)()) { cb = h; }
HOLDER(HolderNew)
HOLDER(HolderShared)
HOLDER(HolderUnique)
HOLDER(HolderSptr)
HOLDER(HolderRet)
HOLDER(HolderArg)
HOLDER(HolderVolatile)
void holder_new() {
    auto p = new HolderNew(Handler);
    p->cb();
}
void holder_shared() {
    auto s = std::make_shared<HolderShared>(Handler);
    s->cb();
}
void holder_unique() {
    auto s = std::make_unique<HolderUnique>(Handler);
    s->cb();
}
void holder_sptr() {
    OHOS::sptr<HolderSptr> s = OHOS::sptr<HolderSptr>::MakeSptr(Handler);
    s->cb();
}
std::shared_ptr<HolderRet> holder_make() { return std::make_shared<HolderRet>(Handler); }
void holder_returned() {
    auto s = holder_make();
    s->cb();
}
void holder_run(std::shared_ptr<HolderArg> s) { s->cb(); }
void holder_nested() { holder_run(std::make_shared<HolderArg>(Handler)); }
// cv-qualifiers on the template argument are dropped, `volatile` too.
void holder_volatile() {
    auto s = std::make_shared<volatile HolderVolatile>(Handler);
    s->cb();
}

// ---- Braces construct as parentheses do when the class has a constructor ----
void by_new_braced() { auto c = new Foo{3, Handler}; }

// ---- A project's own `make_shared` shadows the standard one ----
// The bare call resolves to it, so it is not the standard factory and
// constructs nothing (#192 review).
namespace project {
template <class T, class... Args> T *make_shared(Args... args);
}
namespace shadowed {
using namespace std;
using namespace project;
void own_factory() { auto p = make_shared<Foo>(1, Handler); }
} // namespace shadowed
namespace project {
void in_project() { auto p = make_shared<Foo>(1, Handler); }
} // namespace project

// ---- A copy or a move runs the implicit constructor ----
// One argument no user-provided constructor takes is the object copied:
// no constructor site, through `new` or a factory (#192 review).
void copy_new(Foo *f) { auto c = new Foo(*f); }
void copy_shared(Foo *f) { auto c = std::make_shared<Foo>(*f); }
void copy_sptr(Foo *f) { auto c = OHOS::sptr<Foo>::MakeSptr(*f); }

// ---- Placement arguments are not the constructor's ----
void by_new_nothrow() { auto c = new (std::nothrow) Foo(3, Handler); }
void copy_new_nothrow(Foo *f) { auto c = new (std::nothrow) Foo(*f); }

// ---- `new` as a statement constructs as a factory statement does ----
void by_new_statement() { new Foo(1, Handler); }

// ---- A file-scope factory has no constructor site and makes no object ----
// `Foo` has a user-provided constructor: in a body this call would make one.
auto g_holder = std::make_shared<Foo>(1, Handler);
