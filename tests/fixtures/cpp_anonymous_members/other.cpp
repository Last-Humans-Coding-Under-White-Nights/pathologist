// Another file's anonymous namespace declares a class of the same name: a
// different class, whose members stay apart from the first file's.
#include "shared_statics.h"

void OnSharedThere() {}

static void SharedTag(double x) { OnSharedThere(); }

void UseSharedThere(SharedArg a) {
    SharedTag(1.5);
    SharedDeclared(a);
}

static void SharedDeclared(SharedArg a) { OnSharedThere(); }

namespace {
class Profile {
public:
    int Ratio() { return 3; }
};
} // namespace

void DriveOther() {
    Profile p;
    p.Ratio();
}

// So does a virtual call on one: it reaches this file's override only.
class Iface {
public:
    virtual void Fire() {}
};

void OnHeaderFire() {}
void OnHeaderLogInt() {}
void OnHeaderLogDouble() {}
void OnHeaderLeaf() {}

#include "anon_header.h"

void HeaderImpl::Fire() { OnHeaderFire(); }

namespace {
struct HeaderLeaf : HeaderImpl {
    void Fire() override { OnHeaderLeaf(); }
};
} // namespace

// The definition written here does not repeat `virtual`; the header's
// prototype does.
void FireThroughHeaderImpl(HeaderImpl *h) { h->Fire(); }

static void HeaderLog(double x) { OnHeaderLogDouble(); }

void UseHeaderLog() {
    void (*p)(int) = HeaderLog;
    p(1);
}

void FireHeaderImpl() {
    HeaderImpl h;
    h.Fire();
}

void OnOtherFired() {}
void OnQuiet() {}

namespace {
class Impl : public Iface {
public:
    void Fire() override { OnOtherFired(); }
};

class Quiet : public Iface {
public:
    void Fire() override { OnQuiet(); }
};
} // namespace

void OnInheritsSub() {}

namespace {
class Inherits : public Iface {};

class InheritsSub : public Inherits {
public:
    void Fire() override { OnInheritsSub(); }
};

struct Plainly {
    virtual void Run() {}
};
} // namespace

struct Finished : Iface {
    void Fire() override {}
};

struct FinishedSub : Finished {
    void Fire() override {}
};

void FireFinished(Finished *p) { p->Fire(); }

struct Open : Iface {
    void Fire() override {}
};

struct OpenSub : Open {
    void Fire() override {}
};

void FireOpen(Open *p) { p->Fire(); }

struct Sealed : Iface {
    void Fire() override {}
};

struct SealedSub : Sealed {
    void Fire() override {}
};

void FireSealed(Sealed *p) { p->Fire(); }

void FireOther() {
    Impl i;
    i.Fire();
}

struct OtherRoot {
    virtual void Run() {}
};

namespace {
struct Mixed : OtherRoot {};
} // namespace

void CallMixedThere(Mixed *m) { m->Run(); }

namespace {
struct Shared : OtherRoot {
    void Run() override {}
};

struct Named {
    virtual void Run() {}
};

struct NamedSub : Named {
    void Run() override {}
};
} // namespace

void CallOtherRoot(OtherRoot *r) { r->Run(); }
