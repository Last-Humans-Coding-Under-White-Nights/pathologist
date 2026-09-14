struct Worker { void run() {} virtual void go() {} };
struct Sub : Worker { void go() override {} };
Worker *get_worker();
Worker *defined_worker() { return nullptr; }
struct Factory { static Worker *get(); Worker *member() { return nullptr; } };
void typed() { Worker *p = get_worker(); p->run(); p->go(); }
void inferred() { auto p = get_worker(); p->run(); p->go(); }
void chained() { auto q = get_worker(); auto r = q; r->run(); r->go(); }
void references() { auto q = get_worker(); auto& r = q; const auto& s = r; s->run(); s->go(); }
void defined() { auto p = defined_worker(); p->run(); p->go(); }
void qualified() { auto p = Factory::get(); p->run(); p->go(); }
void member(Factory *f) { auto p = f->member(); p->run(); p->go(); }
void conditional() { if (auto p = get_worker()) { p->run(); p->go(); } }
void init_statement() { if (auto p = get_worker(); p) { p->run(); p->go(); } }
Worker &ref_worker();
void returned_reference() { auto& p = ref_worker(); p.run(); p.go(); }
void copied_reference() { auto p = ref_worker(); p.run(); p.go(); }
void lock_typed(std::weak_ptr<Worker> w) { std::shared_ptr<Worker> p = w.lock(); p->run(); p->go(); }
void lock_auto(std::weak_ptr<Worker> w) { auto p = w.lock(); p->run(); p->go(); }
void promote_typed(OHOS::wptr<Worker> w) { OHOS::sptr<Worker> p = w.promote(); p->run(); p->go(); }
void promote_auto(OHOS::wptr<Worker> w) { auto p = w.promote(); p->run(); p->go(); }
void shared_typed() { std::shared_ptr<Worker> p = std::make_shared<Worker>(); p->run(); p->go(); }
void shared_auto() { auto p = std::make_shared<Worker>(); p->run(); p->go(); }
void unique_typed() { std::unique_ptr<Worker> p = std::make_unique<Worker>(); p->run(); p->go(); }
void unique_auto() { auto p = std::make_unique<Worker>(); p->run(); p->go(); }
struct Other { void run() {} };
namespace factories {
struct A {};
struct B {};
Worker &select(A);
Other &select(B);
}
Worker &factories::select(A a) { static Worker w; return w; }
Other &factories::select(B b) { static Other o; return o; }
void qualified_definition(factories::A a) { auto& p = factories::select(a); p.run(); p.go(); }
void global_qualified(factories::A a) { auto& p = ::factories::select(a); p.run(); p.go(); }
void global_free() { auto p = ::get_worker(); p->run(); p->go(); }
void global_shared() { auto p = ::std::make_shared<Worker>(); p->run(); p->go(); }
// The body of a definition spelled `N::f` looks names up in `N`, as its
// parameters do.
namespace factories {
struct Helper { Worker *get(); };
void body_scope();
}
void factories::body_scope() { Helper h; auto p = h.get(); p->run(); p->go(); }
// A value parameter is named by its declarator, not its type, and a
// qualified name is not a parameter of the same spelling.
template<Worker *W> Worker *pinned();
void value_parameter() { auto p = pinned<nullptr>(); p->run(); p->go(); }
template<class Worker> ::Worker *global_spelling();
void qualified_parameter_spelling() { auto p = global_spelling<int>(); p->run(); p->go(); }
Worker *ambiguous(int);
Other *ambiguous(double);
void unresolved() { auto p = ambiguous(unknown()); p->run(); auto q = missing(); q->run(); }
Worker *partial(int, int);
Other *partial(double, double);
void partial_unknown(int x) { auto p = partial(x, unknown()); p->run(); }
Other *hidden_factory();
struct Owner {
    Worker *hidden_factory();
    void implicit_member() { auto p = hidden_factory(); p->run(); p->go(); }
};
// A real class with the parameter's spelling makes a guessed type observable.
struct T { void run() {} };
template<class T> T *dependent();
template<class T> void dependent_use() { auto p = dependent<T>(); p->run(); }
template<class T> void dependent_shared() { auto p = std::make_shared<T>(); p->run(); }
template<class T> void dependent_copy(T *q) { auto p = q; p->run(); }
template<class T> void dependent_lock(std::weak_ptr<T> w) { auto p = w.lock(); p->run(); }
struct N { void run() {} };
template<int N> N *dependent_value();
void dependent_value_use() { auto p = dependent_value<1>(); p->run(); }
// Lowering qualifies a parameter it cannot find into the enclosing namespace
// (`box::D`), so a qualified lowered name can still be the parameter.
namespace box {
struct Box {
    template<class D> void dependent_member() { auto p = new D(); p->run(); }
};
}
// Overloads that agree on the return type give it, arguments known or not.
struct Store { Worker *get(); Worker *get() const; };
void agreeing_overloads(Store *s) { auto p = s->get(); p->run(); p->go(); }
Worker *agree(int);
Worker *agree(double);
void agreeing_unknown() { auto p = agree(unknown()); p->run(); p->go(); }
// A bare factory is the standard one only where `std` is brought in.
namespace uses_std {
using namespace std;
void bare_shared() { auto p = make_shared<Worker>(); p->run(); p->go(); }
}
void bare_without_using() { auto p = make_shared<Worker>(); p->run(); }
// A receiver or scope typed by a template parameter has no members yet.
struct M { Worker *make(); static Worker *create(); };
template<class M> void dependent_receiver(M *m) { auto p = m->make(); p->run(); }
template<class M> void dependent_static() { auto p = M::create(); p->run(); }
template<class M> struct Holder {
    M *held;
    void use() { auto p = held->make(); p->run(); }
};
// A declarator asking for more pointer layers than the initializer has
// deduces nothing.
Worker value_worker();
void pointer_to_value() { auto *p = value_worker(); p->run(); }
void pointer_depth_mismatch() { auto **p = get_worker(); (*p)->run(); }
// `T &r(a);` binds a reference: nothing is constructed.
struct Widget { Widget(const Widget &); };
void reference_direct_init(Widget &other) { Widget &w(other); }
// A member alias of a class template stands for a dependent type too.
struct Event { void run() {} };
template<class Event> struct Handler { using Ptr = std::shared_ptr<Event>; Ptr Get(); };
void dependent_alias(Handler<int> &h) { auto p = h.Get(); p->run(); }
// A type lowering could not resolve reads as a scalar stand-in: no guess.
Unresolved GetBare();
void placeholder_scalar() { auto p = GetBare(); }
// A weak pointer spelled in a namespace upgrades in that namespace; a
// cv-qualified argument still names its class.
namespace cam {
void promote_in_namespace(wptr<Worker> w) { auto p = w.promote(); p->run(); p->go(); }
void shared_const() { auto p = std::make_shared<const Worker>(); p->run(); p->go(); }
}
template<class U> struct Box { void run() {} };
void shared_template() { auto p = std::make_shared<Box<int>>(); p->run(); }
// A smart pointer's argument that names a class only through `using` is
// ambiguous with the spelling the index holds: no guess.
namespace hdfx { struct Strategy { void run() {} }; }
namespace strat_user {
using namespace hdfx;
std::shared_ptr<Strategy> MakeStrategy();
void via_using() { auto p = MakeStrategy(); p->run(); }
}
// `cb && cb();` is an expression, not a reference-returning prototype.
using Callback = void (*)();
void misparse(Callback cb) { cb && cb(); Callback copy = cb; copy(); }
// A C-style cast keeps its pointer layer.
void cast_auto(void *v) { auto p = (Worker *)v; p->run(); p->go(); }
// A base that is a template parameter has no members yet.
struct SelfBase { Worker *self(); };
template<class SelfBase> struct Mixin : SelfBase {
    void through_this() { auto p = this->self(); p->run(); }
    void implicit() { auto p = self(); p->run(); }
};
// `T(args)` constructs a `T`, as the call site reads it.
Other *Gadget(int);
namespace ui {
struct Gadget { Gadget(int) {} void run() {} };
void construct_local() { auto p = Gadget(1); p.run(); }
}
// A nested type is found in a base before the scopes around the class.
struct Node { void run() {} };
struct NodeBase { struct Node { void run() {} }; };
struct NodeDerived : NodeBase { Node *first(); };
void inherited_nested(NodeDerived &d) { auto p = d.first(); p->run(); }
// `new ns::T` names `ns::T`, not a parameter spelled `T`.
namespace qual { struct Maker { void run() {} }; }
template<class Maker> void new_qualified() { auto p = new qual::Maker(); p->run(); }
// The body of `N::f` looks function names up in `N`.
namespace bodyns { Worker *make(); void body(); }
void bodyns::body() { auto p = make(); p->run(); p->go(); }
// A local lives until the end of its scope.
void shadowed_scope(Other *p) { if (auto p = get_worker()) { p->run(); } p->run(); }
// A condition's variable starts where its declarator does.
void condition_spans() { if (Worker *typed = get_worker()) {} if (auto inferred = get_worker()) {} }
