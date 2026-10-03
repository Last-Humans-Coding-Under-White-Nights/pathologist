// Issue #186: constructors and destructors of automatic C++ objects.
// The first block is the issue's reproducer verbatim.
void on_ctor(); void on_dtor();
struct Guard {
    Guard() { on_ctor(); }
    explicit Guard(int) { on_ctor(); }
    ~Guard() { on_dtor(); }
};
void on_ctor() {} void on_dtor() {}
void plain()  { Guard g; }
void braces() { Guard g{}; }
void parens() { Guard g(1); }
void heap()   { Guard *g = new Guard(); delete g; }
int early(int x) { Guard g(1); if (x) return 1; return 0; }

// Locals in nested blocks, init statements and lambda bodies.
void nested() { { Guard g; } }
void for_init() { for (Guard g; ; ) break; }
void if_init(int x) { if (Guard g; x) {} }

struct Flag {
    explicit Flag(int) {}
    ~Flag() {}
    explicit operator bool() const { return true; }
};
void cond() { if (Flag f = Flag(1)) {} }

void lam() { auto f = [] { Guard g; }; f(); }

// An array of class type: one constructor site and one destructor site.
void array() { Guard a[4]; }

// A virtual destructor whose body calls through the implicit `this`.
struct VBase {
    virtual ~VBase();
    virtual void Hook();
};
VBase::~VBase() { Hook(); }
void VBase::Hook() {}
void vlocal() { VBase v; }

// Negatives: trivial and defaulted members, references and pointers; static
// and thread_local locals get no destructor site.
struct Trivial { int x; };
void triv() { Trivial t; }

struct Defaulted {
    Defaulted() = default;
    ~Defaulted() = default;
};
void defl() { Defaulted d; }

void refs(Guard &in) { Guard &r = in; Guard *p = &in; }
void statics() { static Guard s; }
void tls() { thread_local Guard t; }

// A class without its own constructor or destructor reaches its base's, as
// `D d{};` already does.
struct Derived : Guard {};
void derived() { Derived d; }
void derived_braces() { Derived d{}; }

// A range-for variable and a caught-by-value exception are destroyed; both
// are copy-initialized, so no default constructor runs.
void rangefor(Guard (&arr)[2]) { for (Guard g : arr) {} }
void catcher() { try {} catch (Guard g) {} }
void rangeref(Guard (&arr)[2]) { for (Guard &g : arr) {} }
void catchref() { try {} catch (Guard &g) {} }

// A placeholder loop variable takes the element type of the array or standard
// container it iterates.
struct Param { virtual int Kind(); };
struct SubParam : Param { int Kind() override; };
int Param::Kind() { return 0; }
int SubParam::Kind() { return 1; }
void autoloop(Param *(&params)[2]) {
    for (const auto &param : params) { param->Kind(); }
}

// A defaulted constructor or destructor runs the base's.
struct DefaultedDerived : Guard {
    DefaultedDerived() = default;
    ~DefaultedDerived() = default;
};
void defaulted_derived() { DefaultedDerived d; }
struct VirtualDefaulted : Guard { virtual ~VirtualDefaulted() = default; };
void virtual_defaulted() { VirtualDefaulted d; }

// A loop variable of a template parameter type names no class: the lambda's
// `Param *param` is out of scope after the lambda, so no call resolves.
template <class T> void tloop(T (&params)[2]) {
    auto f = [] (Param *param) { (void)param; };
    for (T param : params) { param->Kind(); }
}

// The range expression is evaluated before the loop variable exists.
struct Item { ~Item(); void list(); };
Item::~Item() {}
void Item::list() {}
struct Items { Item *begin(); Item *end(); };
struct Box { Items list(); };
Items Box::list() { return {}; }
void shadow(Box item) { for (Item item : item.list()) {} }

// A loop variable pointing to a known class resolves the call through it.
struct Handler { virtual void Run(); };
void Handler::Run() {}
void handlers(Handler *(&hs)[2]) { for (Handler *h : hs) { h->Run(); } }

// A placeholder-typed loop variable hides the outer name it shadows.
struct Other { int Kind(); };
int Other::Kind() { return 2; }
void shadow_auto(Param *param, Other *(&others)[2]) {
    for (auto *param : others) { param->Kind(); }
}

// A block-scope function declaration's or a lambda's parameter is not a local
// of the enclosing function.
Param *gparam;
void decl_scope() {
    void take(Other *gparam);
    gparam->Kind();
}
void lambda_scope() {
    auto f = [] (Other *gparam) { (void)gparam; };
    gparam->Kind();
}

namespace std {
template <class T> class shared_ptr { public: T *operator->() const; };
template <class T> class vector { public: T *begin(); T *end(); };
}
void containers(const std::vector<std::shared_ptr<Other>> &os) {
    for (const auto &o : os) { o->Kind(); }
}

// A loop inside a lambda that initializes a thread_local variable is not
// thread_local itself.
void tl_outer(Guard (&arr)[2]) {
    thread_local int x = [&arr] { for (Guard g : arr) {} return 0; }();
}

// An aggregate whose defaulted constructor is not user-provided is
// initialized by its braces, not constructed, even when its base provides a
// constructor.
void handler() {}
struct Cfg : Guard {
    Cfg() = default;
    void (*cb)();
};
void aggregate() { Cfg c{{}, handler}; c.cb(); }

// Every base's members run, not only the first one found.
struct BaseA { BaseA(); ~BaseA(); };
struct BaseB { BaseB(); ~BaseB(); };
struct TwoBases : BaseA, BaseB {};
void two_bases() { TwoBases d; }

// A defaulted `D()` runs the base's constructor, beside a user-provided
// overload, a constructor template or an ellipsis constructor that could also
// take no arguments: overload resolution picks `D()`.
struct DefaultPlus : Guard {
    DefaultPlus() = default;
    explicit DefaultPlus(int);
};
void default_plus() { DefaultPlus d; }
struct TemplateCtor : Guard {
    TemplateCtor() = default;
    template <class... A> TemplateCtor(A... a);
};
void template_ctor() { TemplateCtor d; }
struct EllipsisCtor : Guard {
    EllipsisCtor() = default;
    EllipsisCtor(...);
};
void ellipsis_ctor() { EllipsisCtor d; }

// A union with user-provided members is constructed and destroyed too.
union Variant {
    Variant();
    explicit Variant(int);
    ~Variant();
    int i;
};
void unions() { Variant v; }
void union_braces() { Variant v{}; }
void union_parens() { Variant v(1); }
struct HoldsVariant {
    HoldsVariant();
    Variant v_;
};
HoldsVariant::HoldsVariant() : v_(1) {}

// A defaulted destructor runs its base's, which does not dispatch to the
// base's other subclasses.
struct VB { virtual ~VB(); };
struct VD : VB { ~VD() = default; };
struct VE : VB { ~VE(); };
void sibling() { VD d; }

// An unnamed caught-by-value exception is destroyed too; an unnamed
// reference names no object.
void catch_unnamed() { try {} catch (Guard) {} }
void catch_unnamed_ref() { try {} catch (Guard &) {} }

// A structured binding is not lowered, by reference neither.
struct Pair { int x; int y; };
void bindings(Pair (&ps)[2]) {
    for (auto &[x, y] : ps) {}
    for (const auto &[a, b] : ps) {}
    for (auto &&[c, d] : ps) {}
}

// A placeholder reference to a pointer element keeps the pointer.
void ptr_refs(Handler *(&hs)[2]) { for (const auto &h : hs) { h->Run(); } }

// Const-qualified elements, a qualified range and a `::std::` container.
void const_shared(const std::vector<const std::shared_ptr<Other>> &os) {
    for (const auto &o : os) { o->Kind(); }
}
void const_ptrs(const std::vector<Other *const> &os) { for (auto o : os) { o->Kind(); } }
namespace lns { std::vector<std::shared_ptr<Other>> items; }
void qualified_range() { for (auto &o : lns::items) { o->Kind(); } }
void global_std(const ::std::vector<std::shared_ptr<Other>> &os) {
    for (auto &o : os) { o->Kind(); }
}

// Bases whose default constructors differ in shape each get their own site.
struct BaseC { BaseC(int x = 0); ~BaseC(); };
struct MixedBases : BaseA, BaseC {};
void mixed_bases() { MixedBases d; }

// Empty braces value-initialize: they construct as the bare declaration does,
// arrays included.
void two_bases_braces() { TwoBases d{}; }
void default_plus_braces() { DefaultPlus d{}; }
void array_braces() { Guard a[4]{}; }
void array_equals() { Guard a[4] = {}; }

// A condition declaration's braces name its constructor.
void cond_braces() { if (Flag f{1}) {} }

// A C-style union is copied, not constructed.
union Raw { void *p; long l; };
void raw_copy(Raw src) { Raw r(src); }

// A loop over an array reads its elements from the array summary.
void tab1() {}
void tab2() {}
typedef void (*tab_fn)();
tab_fn table[2] = {tab1, tab2};
void loop_table() { for (auto f : table) { f(); } }

// A bare container under `using namespace std;` (no standard header indexed).
using namespace std;
void bare_list(const list<Other *> &os) { for (auto o : os) { o->Kind(); } }
