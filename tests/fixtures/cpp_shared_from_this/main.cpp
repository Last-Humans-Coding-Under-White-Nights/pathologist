// `shared_from_this()` / `weak_from_this()` yield the object itself
// (docs/ANALYSIS.md, "`shared_from_this`"). The stub declares the standard's
// const twins, whose results disagree, as libstdc++ and libc++ do.
namespace std {
template <class T> class shared_ptr { public: T *operator->() const; };
template <class T> class weak_ptr { public: shared_ptr<T> lock(); };
template <class T> class enable_shared_from_this {
public:
    shared_ptr<T> shared_from_this();
    shared_ptr<const T> shared_from_this() const;
    weak_ptr<T> weak_from_this() noexcept;
    weak_ptr<const T> weak_from_this() const noexcept;
};
}

class Manager : public std::enable_shared_from_this<Manager> {
public:
    void Handle();
    void ViaShared() { auto self = shared_from_this(); self->Handle(); }
    void ViaWeak() { auto w = weak_from_this(); auto self = w.lock(); self->Handle(); }
    void InitCapture() {
        auto task = [self = shared_from_this()]() { self->Handle(); };
        task();
    }
    void InitCaptureWeak() {
        auto task = [w = weak_from_this()]() { auto self = w.lock(); self->Handle(); };
        task();
    }
    void CaptureThis() {
        auto task = [this]() { shared_from_this()->Handle(); };
        task();
    }
    std::shared_ptr<Manager> Self() { return shared_from_this(); }
};
void Manager::Handle() {}

// An explicit receiver: the pointer, or a reference's referent, is the object.
void Explicit(Manager *p, Manager &r) {
    auto via_arrow = p->shared_from_this();
    via_arrow->Handle();
    auto via_ref = r.weak_from_this();
    auto via_deref = (*p).shared_from_this();
}

// The base holds `C`; a derived class's call still yields `shared_ptr<Base>`.
class Base : public std::enable_shared_from_this<Base> {
public:
    virtual void Notify();
};
class Derived : public Base {
public:
    void Notify() override;
    void Run() { auto inherited = shared_from_this(); inherited->Notify(); }
};
void Base::Notify() {}
void Derived::Notify() {}

// Not an `enable_shared_from_this`: its own member is an ordinary call.
class Impostor {
public:
    Impostor *shared_from_this();
    void Handle();
    void Run() { auto own = shared_from_this(); own->Handle(); }
};
void Impostor::Handle() {}

void drive(Manager *m, Derived *d, Impostor *i) {
    m->ViaShared();
    m->ViaWeak();
    m->InitCapture();
    m->InitCaptureWeak();
    m->CaptureThis();
    Explicit(m, *m);
    d->Run();
    i->Run();
}
