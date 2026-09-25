// Subclasses through template-parameter bases (#150). Wrappers are spelled
// without their headers, as in the other smart-pointer fixtures.

struct IFoo {
    void (*cb)();
    virtual void Handle();
};
void IFoo::Handle() {}

namespace OHOS {
template <class I> class IRemoteStub : public I {};
}

// The OpenHarmony stub shape: IFoo reaches FooService only as IRemoteStub's `I`.
class FooService : public OHOS::IRemoteStub<IFoo> {
public:
    void Handle() override;
};
void FooService::Handle() {}

// Control: a direct subclass, recognized today.
struct Direct : IFoo {
};

void Stub() { OHOS::sptr<IFoo> svc = new FooService(); svc->cb(); }
void Plain() { OHOS::sptr<IFoo> d = new Direct(); d->cb(); }

// Virtual dispatch through the interface reaches the override.
void Dispatch(IFoo *foo) { foo->Handle(); }

// Nested: BarImpl : Wrapper<IBar>, Wrapper<T> : Layer<T>, Layer<I> : I.
struct IBar { virtual void Run(); };
template <class I> class Layer : public I {};
template <class T> class Wrapper : public Layer<T> {};
class BarImpl : public Wrapper<IBar> {
public:
    void Run() override;
};
void BarImpl::Run() {}
void RunBar(IBar *bar) { bar->Run(); }

// The argument names the class in scope where the derived class is
// declared, not a same-named class elsewhere.
namespace svc {
struct IFoo { virtual void Handle(); };
class Scoped : public OHOS::IRemoteStub<IFoo> {
public:
    void Handle() override;
};
void Scoped::Handle() {}
}

// A member lookup while lowering sees the parameter base: a non-virtual
// member of the argument is found on the derived class.
struct IHelp { void Helper(); };
void IHelp::Helper() {}
class HelpSvc : public OHOS::IRemoteStub<IHelp> {};
void CallHelper(HelpSvc *s) { s->Helper(); }

// A parameter pack binds every remaining argument, one base each.
struct IA { virtual void A(); };
struct IB { virtual void B(); };
void IA::A() {}
void IB::B() {}
template <class... Is> class Multi : public Is... {};
class Impl : public Multi<IA, IB> {
public:
    void A() override;
    void B() override;
};
void Impl::A() {}
void Impl::B() {}
void CallA(IA *a) { a->A(); }
void CallB(IB *b) { b->B(); }

// An omitted argument takes the parameter's default, named in the
// template's scope rather than the derived class's.
namespace tmpl {
struct IDef { virtual void D(); };
void IDef::D() {}
template <class I, class D = IDef> class Def : public I, public D {};
}
class WithDef : public tmpl::Def<IA> {
public:
    void D() override;
};
void WithDef::D() {}
void CallD(tmpl::IDef *d) { d->D(); }

// A nested template's arguments are its own list, not its outer class's.
struct IC { virtual void C(); };
void IC::C() {}
template <class A> struct Outer {
    template <class B> class In : public B {};
};
class Nested : public Outer<IA>::In<IC> {
public:
    void C() override;
};
void Nested::C() {}
void CallC(IC *c) { c->C(); }

// A default written on a forward declaration, which the definition may
// not repeat.
namespace fwd {
struct IFwd { virtual void F(); };
void IFwd::F() {}
template <class I, class D = IFwd> class Def;
template <class I, class D> class Def : public I, public D {};
}
class WithFwdDef : public fwd::Def<IA> {
public:
    void F() override;
};
void WithFwdDef::F() {}
void CallF(fwd::IFwd *f) { f->F(); }

// A base spelled through a template template parameter names the argument
// template, instantiated.
template <template <class> class B, class T> class Wrap : public B<T> {};
class SvcC : public Wrap<Layer, IC> {
public:
    void C() override;
};
void SvcC::C() {}

// A member body's bare names find the argument's members through the
// template: `y` is `IField`'s, and `x` the template's own, which hides the
// argument's `x` as the nearer base.
struct IField { int *x; int *y; };
template <class I> class FieldStub : public I {
public:
    int *x;
};
class FieldSvc : public FieldStub<IField> {
public:
    void Set(int *p) {
        x = p;
        y = p;
    }
};

// A pack forwarded inside a template's arguments: `B<Ts...>` is
// `B<IA, IB>`, all of the pack at once, and `B<Us...>` reaches each.
template <class... Us> struct B : Us... {};
template <class... Ts> struct Pack : B<Ts...> {};
struct S : Pack<IA, IB> {};
template <class... Ts> struct Mixed : B<IC, Ts...> {};
struct SMixed : Mixed<IA> {};

// A pack expanded through a pattern: `B<Lift<Ts>...>` is
// `B<Lift<IA>, Lift<IB>>`.
template <class T> struct Lift : T {};
template <class... Ts> struct Lifted : B<Lift<Ts>...> {};
struct SLifted : Lifted<IA, IB> {};
