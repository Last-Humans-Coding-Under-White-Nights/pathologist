// Bases through class templates the tree does not define (#122).
struct IFoo {
    virtual int Do() = 0;
};

// Forward-declared only: the body is in the ipc component.
template <typename T> class IRemoteProxy;

class FooProxy : public IRemoteProxy<IFoo> {
public:
    int Do() override;
};
int FooProxy::Do() { return 1; }

// Not declared at all.
class FooStub : public IRemoteStub<IFoo> {
public:
    int Do() override { return 2; }
};

class FooMock : public IFoo {
public:
    int Do() override { return 3; }
};

int call(IFoo *f) { return f->Do(); }

// A declared dependent base keeps working (#154).
struct IBar {
    virtual int Go() = 0;
};
template <typename T> class DeclaredProxy : public T {};
class BarProxy : public DeclaredProxy<IBar> {
public:
    int Go() override { return 4; }
};
int callbar(IBar *b) { return b->Go(); }

// A template the tree defines decides, whatever it inherits.
struct IBaz {
    virtual void Run() = 0;
};
struct Other {};
template <class T> struct Plain {};
template <class T> struct Unrelated : public Other {};
class BazPlain : public Plain<IBaz> {
public:
    void Run() {}
};
class BazUnrelated : public Unrelated<IBaz> {
public:
    void Run() {}
};
void callbaz(IBaz *b) { b->Run(); }

// Guesses that are rejected.
class Crtp : public IRemoteProxy<Crtp> {};
class CycleB;
class CycleA : public IRemoteProxy<CycleB> {
public:
    virtual void Cycle();
};
class CycleB : public IRemoteProxy<CycleA> {
public:
    virtual void Cycle();
};
class Prim : public IRemoteProxy<int> {};
class Multi : public Pair2<IFoo, IBar> {};
class SSvc : public DelayedSingleton<SSvc> {};

namespace ns {
struct IScoped {
    virtual void X() = 0;
};
class ScopedProxy : public IRemoteProxy<IScoped> {
public:
    void X() override {}
};
void callscoped(IScoped *s) { s->X(); }
}

// A pointer or cv-qualified argument is not a base.
class PtrProxy : public IRemoteProxy<IFoo *> {
public:
    int Do() { return 6; }
};
class RefProxy : public IRemoteProxy<const IFoo &> {
public:
    int Do() { return 7; }
};

// Without an override of the argument's members, an out-of-tree template
// is a container or fixture, not an interface wrapper.
struct Handler {
    virtual void Handle() = 0;
};
struct Case {
    int value;
};
class Registry : public std::vector<Handler> {
public:
    void Add() {}
};
class CaseTest : public testing::TestWithParam<Case> {
public:
    void TestBody() {}
};

// A definition reached through a using-directive decides too.
namespace util {
template <class T> struct Hidden {};
}
using namespace util;
class BazHidden : public Hidden<IBaz> {
public:
    void Run() {}
};

// A stub implements none of its interface; the service deriving from it does.
struct ITask {
    virtual void Perform() = 0;
};
class TaskStub : public IRemoteStub<ITask> {
public:
    int OnRemoteRequest();
};
class TaskService : public TaskStub {
public:
    void Perform() override {}
};
void calltask(ITask *t) { t->Perform(); }

// A member of a template instance is not the template: no guess.
template <class T> struct Traits {
    using Base = Other;
};
class TraitImpl : public Traits<IBaz>::Base {
public:
    void Run() {}
};

// An argument whose members all come from its bases is still implemented.
struct IFooAll : IFoo {};
class AllProxy : public IRemoteProxy<IFooAll> {
public:
    int Do() override { return 8; }
};

// No standard template derives from its argument, and a shared name that is
// not virtual in the argument is no evidence.
struct Info {
    virtual int Marshalling();
};
class InfoList : public std::vector<Info> {
public:
    int Marshalling();
};
struct Plainer {
    int Handle();
};
class PlainerBox : public Box<Plainer> {
public:
    int Handle();
};

// A file-local class keeps no global guessed base.
namespace {
class AnonProxy : public IRemoteProxy<IFoo> {
public:
    int Do() override { return 9; }
};
}

// A member template of an instance is not the enclosing template either.
template <class T> struct Outer {
    template <class U> struct Inner {};
};
class NestedImpl : public Outer<IBaz>::Inner<IBaz> {
public:
    void Run() {}
};

// A global class sharing its name with another file's file-local class.
class TwinProxy : public IRemoteProxy<IFoo> {
public:
    int Do() override { return 10; }
};

// An argument that is itself a template instance is not a base.
template <class T> struct Holder {
    virtual int Do();
};
class NestProxy : public IRemoteProxy<Holder<IFoo>> {
public:
    int Do() { return 12; }
};
