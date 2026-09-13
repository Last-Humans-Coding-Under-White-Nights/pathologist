// A class declared inside another is `Outer::Inner`, with a layout of its
// own, and its members stay off the outer class (#92).
namespace CS {
template<class T> class Defined {
public:
    int df;
    class Iterator { public: int it; int Next() { return 1; } };
};
namespace Sub {
int P7(Defined<int>::Iterator i) { int got = i.it; return got + i.Next(); }
}

class Plain {
public:
    class It {
    public:
        int pit;
        void (*cb)();
        int Step();
    };
    template<class U> class Box { public: int Open() { return 2; } };
    It Make();
    int Use() {
        It x;
        return x.pit + x.Step();
    }
};
int Plain::It::Step() { return pit; }
int OpenBox(Plain::Box<int> b) { return b.Open(); }
}

// A struct C would accept keeps its C tag, which C gives file scope, so a
// header shared with C units names it alike; `GOuter::GIn` still names it.
struct GOuter {
    struct GIn { void (*hook)(); };
};
// A body only C++ accepts nests its member classes.
struct GMethods {
    void M();
    struct GIn2 { void (*hook2)(); };
};
void HookTarget() {}
void CallHook(GOuter::GIn g) {
    g.hook = HookTarget;
    g.hook();
}
void CallHook2(GMethods::GIn2 g) {
    g.hook2 = HookTarget;
    g.hook2();
}
void CallNestedCb(CS::Plain::It i) {
    i.cb = HookTarget;
    i.cb();
}

// `T(args)` names a class: a constructor call, also from inside a member
// class, where the outer class is no longer the implicit `this`.
class Built {
public:
    class Builder { public: Built Build() const; };
    explicit Built(const Builder &b) {}
};
Built Built::Builder::Build() const { return Built(*this); }
Built MakeBuilt(const Built::Builder &b) { return Built(b); }
