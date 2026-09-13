// Type lookup corners: constructor arguments, scoping and alias shapes.
typedef void (*PlainCb)();
void CtorCbTarget() {}
void TableCbTarget() {}

// A constructor call's arguments bind past the implicit `this`.
class Takes { public: Takes(PlainCb cb) { cb(); } };
void ConstructTakes() { Takes(CtorCbTarget); }

// A member class sees the enclosing class's members declared after it.
class Later {
public:
    class Builder { public: Later Build() const { return Later(1); } };
    Later(int v) {}
};

// Aliases declared in a function body are scoped to their block.
class LocalTarget { public: int Run() { return 1; } };
int LocalUsing() { using Local = LocalTarget; Local t; return t.Run(); }
int LocalTypedef() { typedef LocalTarget LT; LT t; return t.Run(); }
int LocalOutOfBlock() { { using Hidden = LocalTarget; } Hidden t; return t.Run(); }

// An array alias keeps its array shape.
struct CbEntry { PlainCb cb; };
using CbTable = CbEntry[2];
CbTable cb_table;
void FillTable() { cb_table[1].cb = TableCbTarget; }
void CallTable() { cb_table[1].cb(); }

// A member alias of a class template is reached through its instantiation.
template<class T> struct Holder { using Ptr = LocalTarget *; };
int TemplateMemberAlias(Holder<int>::Ptr p) { return p->Run(); }

// `::Alias` names the global typedef.
typedef LocalTarget GlobalTargetAlias;
namespace rv { int GlobalAliasUse(::GlobalTargetAlias g) { return g.Run(); } }

// A qualified constructor call.
namespace rv { class Made { public: Made(int) {} class In { public: In(int) {} }; }; }
void QualifiedCtor() { rv::Made(1); rv::Made::In(2); }

// A C-style cast names its class through the lookup.
int CastReceiver(void *p) { return ((LocalTarget *)p)->Run(); }

// A C++-only class nested in a struct C also accepts keeps the whole path.
struct CPlain { struct CMid { struct CDeep { int Go() { return 1; } }; }; };

// A constructor call spelled through a function-local alias.
class Built2 { public: Built2(int) {} };
void LocalAliasCtor() { using Local = Built2; Local(1); }

// A member alias of a template that declares its own `operator->`.
template<class T> class ArrowHolder {
public:
    using Ptr = LocalTarget *;
    T *operator->();
};
int ArrowTemplateMemberAlias(ArrowHolder<int>::Ptr p) { return p->Run(); }

// A body-less `struct Name` inside a class names the member class: a
// reference (`struct ListNode *next;`) finds it, and a forward declaration
// (`struct PimplImpl;`) declares it.
class List {
public:
    struct ListNode { void (*cb)(); struct ListNode *next; };
    ListNode *head;
};
class Pimpl {
public:
    struct PimplImpl;
    PimplImpl *p;
    struct PimplImpl { void (*cb)(); };
};

// A member class defined out of line (`class Manager::Info { ... }`) is the
// one its outer class declared, under the whole qualified name.
namespace hm {
class Manager { public: class Info; void Add(); };
class Manager::Info { public: Info(int) {} int Get() { return 1; } };
void Manager::Add() { Info i(1); i.Get(); }
}

// A function declared in a nearer scope hides a class of the same name, so
// the call is to the function, not a constructor.
struct HiddenCtor { HiddenCtor(int) {} };
namespace hide {
void HiddenCtor(int) {}
void CallHidden() { HiddenCtor(1); }
}

// An alias declared in a function-local class stays in that class.
class OuterAliasTarget { public: int Run() { return 1; } };
class InnerAliasTarget { public: int Run() { return 2; } };
typedef OuterAliasTarget *LeakPtr;
int LocalClassAlias() {
    struct LocalHolder { typedef InnerAliasTarget *LeakPtr; LeakPtr q; };
    LeakPtr p = 0;
    return p->Run();
}

// A class with internal linkage (anonymous namespace) constructs like any other.
namespace {
class LocalHelper { public: LocalHelper(int) {} };
}
void UseLocalHelper() { LocalHelper(1); }

// A function passed by name to `new T(...)` reaches the constructor's parameter.
class Consumer { public: Consumer(PlainCb cb) { cb(); } };
void OnEvent() {}
void MakeConsumer() { Consumer *c = new Consumer(OnEvent); }

// An alias of an alias names the class at the end of the chain.
class ChainTarget { public: int Run() { return 1; } };
using FirstAlias = ChainTarget;
using SecondAlias = FirstAlias;
int AliasChain(SecondAlias s) { return s.Run(); }

// A class local to a member function sees the member function's class.
class Enclosing {
public:
    class Nested { public: int Run() { return 1; } };
    int Method();
};
int Enclosing::Method() {
    struct Local { Nested n; } l;
    return l.n.Run();
}

// A class local to a member function looks in the function's class before
// the namespace, so the class's own nested type wins over a namespace one.
namespace lcns {
class Nested { public: int Wrong() { return 0; } };
class Enclosing2 {
public:
    class Nested { public: int Run() { return 1; } };
    int Method();
};
int Enclosing2::Method() {
    struct Local { Nested n; } l;
    return l.n.Run();
}
}

// A member struct forward-declared in a data-only struct and defined out of
// line keeps its methods together with their definitions.
struct HitTarget { void Hit() {} };
struct CConfig { struct CParser; CParser *parser; int flags; };
struct CConfig::CParser { HitTarget *t; void Parse(); };
void CConfig::CParser::Parse() { t->Hit(); }
