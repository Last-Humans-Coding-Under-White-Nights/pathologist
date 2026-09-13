// Members of a class in an anonymous namespace have internal linkage, and
// member lookup still finds them.
typedef void (*Callback)();

#include "shared_statics.h"

void OnSharedHere() {}

static void SharedTag(double x) { OnSharedHere(); }

void UseSharedHere(SharedArg a) {
    SharedTag(1.5);
    SharedDeclared(a);
}

static void SharedDeclared(SharedArg a) { OnSharedHere(); }

void OnTake() {}
void OnHolder() {}
void OnOut() {}
void OnFired() {}

class Iface {
public:
    virtual void Fire() {}
};

namespace {
class Profile {
public:
    int Ratio() { return 1; }
    int Use() { return Ratio() + Later(); }
    int Later() { return 2; }
    void Take(Callback cb) { cb(); }
};

struct Holder {
    Holder(Callback cb) { cb(); }
};

class Out {
public:
    void Run();
};

void Out::Run() { OnOut(); }

class Impl : public Iface {
public:
    void Fire() override { OnFired(); }
};
} // namespace

void Drive() {
    Profile p;
    p.Use();
    p.Take(OnTake);
    Holder h(OnHolder);
    Out o;
    o.Run();
}

void Dispatch(Iface *i) { i->Fire(); }

void FireHere() {
    Impl i;
    i.Fire();
}

// Overloads of a member of such a class stay two functions.
void OnOne() {}
void OnTwo() {}

namespace {
class Overloaded {
public:
    void Work(int a) { OnOne(); }
    void Work(int a, int b) { OnTwo(); }
};

// Overrides nothing here; other.cpp's class of this name does.
class Quiet : public Iface {};
} // namespace

void DriveOverloads() {
    Overloaded o;
    o.Work(1);
    o.Work(1, 2);
}

void FireQuiet() {
    Quiet q;
    q.Fire();
}

// Same-arity overloads stay two functions as well.
void OnInt() {}
void OnDouble() {}

namespace {
class Typed {
public:
    void Work(int a) { OnInt(); }
    void Work(double a) { OnDouble(); }
};

// Inherits `Fire`; other.cpp's class of this name has a subclass overriding it.
class Inherits : public Iface {};

// `Run` is not virtual here; other.cpp's class of this name declares it virtual.
struct Plainly {
    void Run() {}
};

void OnHides() {}

struct Hides : Plainly {
    void Run() { OnHides(); }
};
} // namespace

void FireInherited() {
    Inherits i;
    i.Fire();
}

void RunPlainly(Plainly *p) { p->Run(); }

// other.cpp declares an external class of this name, with a subclass. This
// one's `final` does not stop dispatch there, and a call here reaches neither.
namespace {
struct Finished : Iface {
    void Fire() final {}
};
} // namespace

void FireOwnFinished(Finished *p) { p->Fire(); }

// Overloads of a free function with internal linkage stay apart as well.
void OnFreeInt() {}
void OnFreeDouble() {}

static void StaticWork(int a) { OnFreeInt(); }
static void StaticWork(double a) { OnFreeDouble(); }

namespace {
void FreeWork(int a) { OnFreeInt(); }
void FreeWork(double a) { OnFreeDouble(); }

// other.cpp declares external classes of these names, with subclasses.
struct Open : Iface {
    void Fire() override {}
};

struct Sealed final : Iface {
    void Fire() override {}
};
} // namespace

void DriveFreeOverloads() {
    StaticWork(1);
    StaticWork(1.5);
    FreeWork(1);
    FreeWork(1.5);
}

void FireOwnOpen(Open *p) { p->Fire(); }

// A function pointer taken from an overloaded name may hold any overload.
void (*free_work_ptr)(double) = FreeWork;

void CallFreeWorkPtr() { free_work_ptr(1.5); }

static void TakeWork(void (*work)(double)) { work(1.5); }

void PassStaticWork() { TakeWork(StaticWork); }

// Members defined after the namespace closes are still internal, and an
// overload defined there is still a member.
namespace {
class Outside {
public:
    void Run(int a);
    void Run(double a);
};
} // namespace

void Outside::Run(int a) { OnFreeInt(); }
void Outside::Run(double a) { OnFreeDouble(); }

// A base without a declared constructor is not constructed through the
// derived constructor that names it.
class Plain {
public:
    int x;
};

class FromPlain : public Plain {
public:
    FromPlain(int a) : Plain() {}
};

namespace {
class AnonFromPlain : public Plain {
public:
    AnonFromPlain(int a) : Plain() {}
};
} // namespace

// An external class sharing its name with another file's anonymous class
// does not inherit that class's bases or subclasses.
struct ExternalRoot {
    virtual void Run() {}
};

struct Shared : ExternalRoot {};

void CallExternalRoot(ExternalRoot *r) { r->Run(); }

struct Named {
    virtual void Run() {}
};

void CallNamed(Named *n) { n->Run(); }

// An anonymous class derives from its own file's bases only.
namespace {
struct Mixed : ExternalRoot {};
} // namespace

void CallMixed(Mixed *m) { m->Run(); }

// A declaration and a definition of types in unrelated namespaces are two
// functions.
namespace cfg1 {
struct Config {
    int a;
};
} // namespace cfg1

namespace cfg2 {
struct Config {
    int b;
};
} // namespace cfg2

void OnLoad() {}

static void Load(cfg1::Config *c);
static void Load(cfg2::Config *c) { OnLoad(); }

// A declaration defined later under its exact signature is not taken by an
// overload of another type defined first.
struct Arg {
    int x;
};

void OnPickInt() {}
void OnPickArg() {}

static void Pick(Arg a);

void EarlyPick(Arg a) { Pick(a); }

static void Pick(int n) { OnPickInt(); }
static void Pick(Arg a) { OnPickArg(); }

// A `static` declaration and its definition spelling a type two ways are one
// function. Kept last: the directive reaches the end of the file.
namespace spelled {
struct Obj {
    int v;
};
} // namespace spelled

using namespace spelled;

void OnRespelled() {}

static void Respelled(spelled::Obj *o);

void CallRespelled(Obj *o) { Respelled(o); }

static void Respelled(Obj *o) { OnRespelled(); }
