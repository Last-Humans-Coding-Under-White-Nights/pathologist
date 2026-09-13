// Out-of-tree wrappers retain their argument for arrow resolution (#86).
class AbsentTarget { public: virtual int Run() { return 1; } };
class AbsentDerived : public AbsentTarget { public: int Run() override { return 2; } };
int AbsentLocal() { missing<AbsentTarget> p; return p->Run(); }
int AbsentParameter(missing<AbsentTarget> p) { return p->Run(); }
class AbsentHolder { public: missing<AbsentTarget> p; };
int AbsentField(AbsentHolder h) { return h.p->Run(); }
OHOS::sptr wrapper_mentioned;
int AbsentQualified(OHOS::sptr<AbsentTarget> p) { return p->Run(); }
int AbsentTwo(OHOS::pair<AbsentTarget, AbsentDerived> p) { return p->Run(); }
int AbsentScalar(OHOS::box<int> p) { return p->Run(); }
int AbsentUnknown(OHOS::box<NotIndexed> p) { return p->Run(); }
int AbsentPointer(OHOS::box<AbsentTarget*> p) { return p->Run(); }
int AbsentReference(OHOS::box<AbsentTarget&> p) { return p->Run(); }
OHOS::NotIndexed mentioned;
int AbsentMentioned(OHOS::box<OHOS::NotIndexed> p) { return p->Run(); }
int AbsentDot(OHOS::sptr<AbsentTarget> p) { return p.promote(); }
class RealTarget { public: int Run() { return 3; } };
template<class T> class DeclaredWrapper { public: RealTarget *operator->(); };
int DeclaredWins(DeclaredWrapper<AbsentTarget> p) { return p->Run(); }
template<class T> class NoArrow { public: int Run() { return 4; } };
int DeclaredNoArrow(NoArrow<AbsentTarget> p) { return p->Run(); }

// An out-of-line `operator->` whose class header is not in the tree is
// still the wrapper's arrow: its return type wins over the guess.
template<class T> RealTarget *OutOfLine<T>::operator->() { return 0; }
int DeclaredOutOfLine(OutOfLine<AbsentTarget> p) { return p->Run(); }

// A forward declaration says nothing about `operator->`: the guess still
// applies, and no member is invented on the wrapper.
template<class T> class ForwardOnly;
int AbsentForward(ForwardOnly<AbsentTarget> p) { return p->Run(); }

// The argument is looked up the way C++ looks a name up: through the
// enclosing namespaces, a partial qualification, a typedef, east const.
namespace Outer {
class Scoped { public: int Run() { return 5; } };
typedef Scoped ScopedAlias;
namespace Inner {
class Deep { public: int Run() { return 6; } };
int AbsentEnclosing(missing<Scoped> p) { return p->Run(); }
int AbsentAlias(missing<ScopedAlias> p) { return p->Run(); }
int AbsentEastConst(missing<Scoped const> p) { return p->Run(); }
}
namespace Other {
int AbsentPartial(missing<Inner::Deep> p) { return p->Run(); }
}
}

// `::T` names the global class and nothing else, and `*` on an undefined
// wrapper is the same guess as `->`.
class Shadow { public: int Run() { return 7; } };
namespace Outer {
namespace Inner {
class Shadow { public: int Run() { return 8; } };
int AbsentShadowed(missing<Shadow> p) { return p->Run(); }
int AbsentGlobal(missing<::Shadow> p) { return p->Run(); }
int AbsentGlobalQualified(missing<::Outer::Scoped> p) { return p->Run(); }
}
}
int AbsentStar(missing<AbsentTarget> p) { return (*p).Run(); }

// A wrapper's nested type is a type of its own, not a wrapper around the
// argument; a scalar or function-type argument is spelled as written.
namespace Outer {
namespace Inner {
int AbsentNestedType(missing<AbsentTarget>::Inner p) { return p->Run(); }
int AbsentSpelledArgs() {
    missing<int> scalar_box;
    missing<void(int, char)> callback_box;
    missing<int(*)(int, int)> fnptr_box;
    return 0;
}
}
}

// The wrapper's own name is looked up through the enclosing namespaces
// too: `OuterBox` inside `Outer::Inner` is `Outer::OuterBox`, whose
// declared arrow and members it keeps.
namespace Outer {
template<class T> class OuterBox { public: Scoped *operator->(); T *Get(); };
namespace Inner {
int DeclaredFromEnclosing(OuterBox<AbsentTarget> p) { p.Get(); return p->Run(); }
}
}

// A typedef is matched by its whole spelling: a qualified alias resolves,
// and an unknown qualified name never lands on an unrelated typedef that
// shares its last segment.
typedef AbsentTarget GlobalHijack;
int AbsentQualifiedAlias(missing<Outer::ScopedAlias> p) { return p->Run(); }
namespace Outer {
namespace Inner {
int AbsentNoHijack(missing<NoSuch::GlobalHijack> p) { return p->Run(); }
}
}

// `sp->f` reads and writes the pointee's field through the wrapper.
class AbsentPayload { public: int absent_payload_value; };
class AbsentBox { public: missing<AbsentPayload> item; };
int AbsentFieldRead(AbsentBox b) { int got = b.item->absent_payload_value; return got; }
void AbsentFieldWrite(AbsentBox b, int v) { b.item->absent_payload_value = v; }

// A field step follows its operator: `sp->f` on a wrapper variable itself
// reaches the pointee's field, while `w.f` stays the wrapper's own field.
int AbsentDirectFieldRead(missing<AbsentPayload> sp) { int got = sp->absent_payload_value; return got; }
class OwnFieldWrapper { public: AbsentPayload *own_raw; AbsentPayload *operator->(); };
class OwnFieldBox { public: OwnFieldWrapper w; };
int WrapperOwnFieldRead(OwnFieldBox b) { AbsentPayload *got = b.w.own_raw; return got != 0; }

// `::missing<T>` names the global wrapper only, tagged without the prefix.
namespace Elsewhere {
int AbsentGlobalHead(::missing<AbsentTarget> p) { return p->Run(); }
}

// A member type of an out-of-tree template keeps its `::` even when an
// unrelated class shares the member's name; it is still no wrapper.
class Cursor { public: int Run() { return 7; } };
int AbsentTemplatedTail(Outer2<AbsentTarget>::Cursor<AbsentTarget> x) { return x->Run(); }

// Standard non-class names and a `T*const` argument are never spelled as
// classes of the enclosing namespace.
namespace Elsewhere {
int AbsentNullptrArg(missing<nullptr_t> p) { return p->Run(); }
int AbsentPtrConstArg(missing<AbsentTarget*const> p) { return p->Run(); }
}

// A member type of a defined template keeps the class the lookup found:
// `Defined<int>::Cursor` inside `Outer::Deep` is `Outer::Defined::Cursor`.
namespace Outer {
template<class T> class Defined { public: int df; class Cursor { public: int Next() { return 1; } }; };
namespace Deep {
int DefinedTail(Defined<int>::Cursor c) { return c.Next(); }
}
}

// A non-type argument is spelled as written, never qualified; `::W<T>`
// as a declared type's head finds that type; a pointer level survives an
// intervening qualifier.
namespace Elsewhere {
int AbsentLiteralArg(missing<AbsentTarget, 4> p) { return p->Run(); }
int AbsentBoolArg(missing<true> p) { return p->Run(); }
int AbsentPtrConstPtrArg(missing<AbsentTarget*const*> p) { return p->Run(); }
}
namespace Outer {
namespace Deep {
int DefinedGlobalHead(::Outer::Defined<int>::Cursor c) { return c.Next(); }
}
}
namespace Outer {
namespace Deep {
int DefinedGlobalField(::Outer::Defined<int> d) { int got = d.df; return got; }
}
}

// A peeled receiver dispatches through every base, not only the first.
class MultiBaseA { public: void FromA() {} };
class MultiBaseB { public: void FromB() {} };
class MultiDerived : public MultiBaseA, public MultiBaseB { public: void Own() {} };
void AbsentMultiFirst(missing<MultiDerived> p) { p->FromA(); }
void AbsentMultiSecond(missing<MultiDerived> p) { p->FromB(); }
void AbsentMultiOwn(missing<MultiDerived> p) { p->Own(); }
