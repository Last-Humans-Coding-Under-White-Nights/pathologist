// PR #206: distinguish object copies from pointer conversions, and resolve
// bare factory names through the same scopes as ordinary calls.
#include "aliases.h"
#include "holder.h"
#include "maker.h"
void Handler();
struct Bar { Bar(void *p); };
Bar::Bar(void *p) {}
void pointer_new(Bar *p) { auto x = new Bar(p); }
void pointer_shared(Bar *p) { auto x = std::make_shared<Bar>(p); }
void pointer_unique(Bar *p) { auto x = std::make_unique<Bar>(p); }
void pointer_allocate(Bar *p, int allocator) { auto x = std::allocate_shared<Bar>(allocator, p); }
void pointer_sptr(Bar *p) { auto x = OHOS::sptr<Bar>::MakeSptr(p); }
void reference_pointer(Bar *&p) { auto x = new Bar(p); }
void cast_reference(Bar &p) { auto x = new Bar((Bar *)&p); }
struct Base {};
struct Derived : Base { Derived(Base *p); };
Derived::Derived(Base *p) {}
void derived_new(Derived *p) { auto x = new Derived(p); }
void derived_shared(Derived *p) { auto x = std::make_shared<Derived>(p); }

struct Parent { Parent(Parent *p); };
Parent::Parent(Parent *p) {}
void implicit_copy(Parent &p) { auto x = new Parent(p); }

struct Own { Own(const Own &o, int flag = 0); };
Own::Own(const Own &o, int flag) {}
void default_copy_new(Own &o) { auto x = new Own(o); }
void default_copy_shared(Own &o) { auto x = std::make_shared<Own>(o); }
void default_copy_local(Own &o) { Own x(o); }
struct Holder { Holder(Own &o); Own own; };
Holder::Holder(Own &o) : own(o) {}
struct Move { Move(Move &&o, int flag = 0); };
Move::Move(Move &&o, int flag) {}
void default_move(Move &o) { auto x = new Move(std::move(o)); }
struct Declared { Declared(const Declared &o, int flag = 0); };
void declared_copy(Declared &o) { auto x = new Declared(o); }
struct Aliased;
using AliasRef = const Aliased &;
struct Aliased { Aliased(AliasRef o, int flag = 0); };
Aliased::Aliased(AliasRef o, int flag) {}
void alias_copy(Aliased &o) { auto x = new Aliased(o); }
void alias_shared(Aliased &o) { auto x = std::make_shared<Aliased>(o); }
void alias_allocate(Aliased &o, int allocator) { auto x = std::allocate_shared<Aliased>(allocator, o); }
using ParentRef = Parent &;
void alias_implicit_copy(ParentRef p) { auto x = new Parent(p); }
void alias_local_copy(Parent &p) { ParentRef r = p; auto x = new Parent(r); }
void alias_block_copy(Parent &p) { using Ref = Parent &; Ref r = p; auto x = new Parent(r); }
void alias_collapsed_param(ParentRef &&p) { auto x = new Parent(p); }
void alias_collapsed_local(Parent &p) { ParentRef &&r = p; auto x = new Parent(r); }
using BarPtr = Bar *;
void alias_pointer(BarPtr &p) { auto x = new Bar(p); }
struct HeaderCopy { HeaderCopy(HeaderRef o, int flag = 0); };
HeaderCopy::HeaderCopy(HeaderRef o, int flag) {}
HeaderImplicit::HeaderImplicit(int value) {}
void header_alias_copy(HeaderCopy &o) { auto x = new HeaderCopy(o); }
void header_alias_implicit(HeaderImplicitRef o) { auto x = new HeaderImplicit(o); }

struct Agg { int x; };
void aggregate_copy(Agg &o) { auto x = new Agg(o); }
struct External;
void external_copy(External &o) { auto x = new External(o); }
void external_shared(External &o) { auto x = std::make_shared<External>(o); }
void external_new_fn() { auto x = new External(Handler); }
void external_shared_fn() { auto x = std::make_shared<External>(Handler); }
template<class T> struct CopyTemplate {
    CopyTemplate(const CopyTemplate<T> &o) {}
};
void template_copy(CopyTemplate<int> &o) { auto x = new CopyTemplate<int>(o); }

struct Foo { Foo(); void run(); };
Foo::Foo() {}
void Foo::run() { Handler(); }
using std::make_shared;
namespace project {
template<class T> T *make_shared();
void shadowed() { auto x = make_shared<Foo>(); x->run(); }
void body_import() { using std::make_shared; auto x = make_shared<Foo>(); x->run(); }
}
void file_import() { auto x = make_shared<Foo>(); x->run(); }
namespace imported {
using std::make_shared;
void namespace_import() { auto x = make_shared<Foo>(); x->run(); }
}
namespace own {
template<class T> T *make_shared();
}
namespace mixed {
using std::make_shared;
void body_shadow() { using own::make_shared; auto x = make_shared<Foo>(); }
}
Widget::Widget(void (*h)()) { h(); }
