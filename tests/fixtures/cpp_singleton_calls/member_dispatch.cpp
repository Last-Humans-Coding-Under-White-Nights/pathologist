// Member overloads chosen on the named class; dispatch to winners' overrides.
// R14-1: an overload only a subclass adds never displaces the base's.
class R14Base {
public:
    virtual void On(int v) {}
};
class R14Derived : public R14Base {
public:
    void On(int v) override {}
    void On(double v) {}
};
void r14_static(R14Base *b) { b->On(1.5); }

// R14-2: an in-tree class is no `int` stand-in for the body redirect.
class R14Foo {};
void R14Set(int v);
void R14Set(R14Foo f);
void R14Set(R14Foo f) {}
void r14_redirect() { R14Set(5); }

// R14-3: `&r` of a reference is the referent's address, not one layer more.
void R14F(R14Foo *p);
void R14F(R14Foo **p);
void r14_address(R14Foo &r) { R14F(&r); }

// R14-7: overloads defined inline in a class template substitute their own
// returns.
class R14Svc {
public:
    void Run() {}
};
template <class T> class R14Wrap {
public:
    T *operator->() const;
};
template <class T> struct R14Box {
    T *Get(int k) { return nullptr; }
    R14Wrap<T> Get(const char *k) { return R14Wrap<T>(); }
};
void r14_inline(R14Box<R14Svc> &b) { b.Get(1)->Run(); }
