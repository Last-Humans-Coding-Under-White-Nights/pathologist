// Specializations of a class template's member must not stop substitution
// for other instantiations (review findings R3-1, R3-2).
class SpecSvc {
public:
    void Run() {}
};
class SpecOther {
public:
    void Run() {}
};

template <class T> class SpecBox {
public:
    T *Get(int key);
};
template <> SpecOther *SpecBox<SpecOther>::Get(int key) { return nullptr; }

template <class T> class ClassSpec {
public:
    T *Get(int key);
};
template <> class ClassSpec<SpecOther> {
public:
    SpecOther *Get(int key);
};

void spec_member(SpecBox<SpecSvc> *b) { b->Get(1)->Run(); }
void spec_class(ClassSpec<SpecSvc> *c) { c->Get(1)->Run(); }

// Distinct same-arity overloads of a plain class keep their own returns
// (R3-4): the argument's type picks one.
class OvBar {
public:
    void Foo() {}
};
class OvBaz {
public:
    void Foo() {}
};
struct Overloaded {
    OvBar *Get(long key);
    OvBaz *Get(int key);
};
void overloaded_int(Overloaded *o, int k) { o->Get(k)->Foo(); }
void overloaded_long(Overloaded *o, long k) { o->Get(k)->Foo(); }

// A member function template's template template parameter is no wrapper
// class either (R3-7), even when a real class shares its name.
class P {
public:
    void Run() {}
};
class MtSvc {
public:
    void Run() {}
};
template <class T> class MtBox {
public:
    template <template <class> class P> static P<T> Get();
};
template <class U> class MtHandle {
public:
    U *operator->() const;
};
void member_template() { MtBox<MtSvc>::Get<MtHandle>()->Run(); }

// An enumerator argument picks the value overload over the pointer one,
// so the chained call keeps its receiver once the two stay apart.
namespace cam {
enum SceneMode { NORMAL = 0, CAPTURE = 1 };
class EnumSession {
public:
    void BeginConfig() {}
};
template <class T> class EnumPtr {
public:
    T *operator->() const;
};
class EnumManager {
public:
    EnumSession *Create(SceneMode mode);
    int Create(EnumPtr<EnumSession> *out);
};
void enum_arg(EnumManager *m)
{
    auto session = m->Create(SceneMode::CAPTURE);
    session->BeginConfig();
}
}

// A member alias naming a class template instance reaches that template's
// own static members.
template <class T> class AliasImpl {
public:
    template <class U> static void Reg(U value) {}
};
class AliasUser {
public:
    using Adaptor = AliasImpl<AliasUser>;
    void Use() { Adaptor::Reg(1); }
};
