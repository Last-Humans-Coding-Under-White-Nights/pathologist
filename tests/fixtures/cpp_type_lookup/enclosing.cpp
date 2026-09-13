// A bare or partially qualified type name is looked up the way C++ looks it
// up: the innermost enclosing namespace first, then each one outward, then
// the global scope (#90).
class GlobalTarget { public: int Run() { return 1; } };

namespace a { namespace b {
class Deep { public: int Go() { return 2; } };
namespace c {
int TriLocal() {
    Deep d;
    return d.Go();
}
int TriParam(Deep &d) { return d.Go(); }
struct Holder { Deep d; };
int TriField(Holder h) { return h.d.Go(); }
Deep *MakeDeep();
int TriGlobal() {
    GlobalTarget g;
    return g.Run();
}
}
} }

namespace a::x {
int Partial(b::Deep d) { return d.Go(); }
}

// The innermost declaration wins over an outer or global one of the same
// name.
class Shadow { public: int Hit() { return 3; } };
namespace n {
class Shadow { public: int Hit() { return 4; } };
namespace m {
int Shadowed(Shadow s) { return s.Hit(); }
int GlobalShadow(::Shadow s) { return s.Hit(); }
}
}

// The return type of a member function, `operator->` included.
class RealTarget { public: int Run() { return 5; } };
namespace Outer {
struct Box { RealTarget *operator->(); };
int ArrowReturn(Box b) { return b->Run(); }
}

// Two namespaces declaring a typedef of the same name keep their own.
namespace p {
class PT { public: int Run() { return 6; } };
typedef PT SameAlias;
}
namespace q {
class QT { public: int Run() { return 7; } };
typedef QT SameAlias;
}
namespace p { namespace inner {
int ScopedTypedef(SameAlias s) { return s.Run(); }
} }
namespace q {
int OtherTypedef(SameAlias s) { return s.Run(); }
}
