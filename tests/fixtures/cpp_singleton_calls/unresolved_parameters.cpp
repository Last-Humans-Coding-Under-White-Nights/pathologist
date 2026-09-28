// Signatures with unresolved parameter types.
class R11A {
public:
    void Go() {}
};
class R11B {
public:
    void Go() {}
};

// R11-1: `Get(Mode)`'s definition joins its own prototype, not `Get(int)`.
struct R11M {
    R11A *Get(int v);
    R11B *Get(Mode m);
};
R11B *R11M::Get(Mode m) { return nullptr; }
R11A *R11M::Get(int v) { return nullptr; }


// R11-4: a call bound to the unresolved-parameter prototype is not redirected
// to the `int` overload's body.
struct R11Redirect {
    static void Set(int v);
    static void Set(Mode m);
};
void R11Redirect::Set(int v) {}
void r11_redirect(Mode m) { R11Redirect::Set(m); }

// An enum global (unresolved enum type, read as `int`) does not bind the
// out-parameter overload, so the chained call keeps its receiver.
class R11Session {
public:
    void Stop() {}
};
template <class T> class R11Ptr {
public:
    T *operator->() const;
};
class R11Manager {
public:
    R11Session *Create(SceneKind kind);
    int Create(R11Ptr<R11Session> *out);
};
SceneKind g_r11_kind;
void r11_enum_global(R11Manager *m)
{
    auto session = m->Create(g_r11_kind);
    session->Stop();
}

// R13-2/R13-3: a virtual call ranked to `On(int)` reaches neither
// `On(double)` nor its override.
class R13Base {
public:
    virtual void On(int v) {}
    virtual void On(double v) {}
};
class R13Derived : public R13Base {
public:
    void On(double v) override {}
};
void r13_dispatch(R13Base *b) { b->On(5); }

// R13-1: `&holder` is a pointer, so the out-parameter overload is kept.
class R13Session {};
template <class T> class R13Ptr {
public:
    T *operator->() const;
};
int R13Create(int flags);
int R13Create(R13Ptr<R13Session> *out);
void r13_address(R13Ptr<R13Session> holder) { R13Create(&holder); }
