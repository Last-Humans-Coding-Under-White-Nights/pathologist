// Overload registration with class-spelled parameters.
namespace r5 {
class Base {
public:
    void Go() {}
};
template <class T> class sptr {
public:
    T *operator->() const;
};

// R5-1: the out-of-line definitions spell `r5::sptr` where the prototypes
// spell `sptr`; each joins its own prototype, not the `int` one.
class Setter {
public:
    void Set(int value);
    void Set(sptr<Base> value);
};
void Setter::Set(r5::sptr<Base> value) {}
void Setter::Set(int value) {}
void set_ptr(Setter *s, sptr<Base> b) { s->Set(b); }
void set_int(Setter *s) { s->Set(1); }
}

// R5-2: classes of one name in two namespaces are two types.
namespace n1 {
struct Config {};
}
namespace n2 {
struct Config {};
}
struct R5Cfg {
    static void Apply(n1::Config c);
    static void Apply(n2::Config c);
};

// R5-4: a template instance is not every type.
template <class T> class R5Vec {};
struct R5Get {
    static void Get(long key);
    static void Get(R5Vec<int> keys);
};

// R5-6: an alias template's return is the pointer it aliases.
class R5Svc {
public:
    void Run() {}
};
template <class U> using R5Ptr = U *;
template <class T> struct R5Box {
    static R5Ptr<T> Get();
};
void r5_alias() { R5Box<R5Svc>::Get()->Run(); }

// An unnamed reference parameter is a reference all the same: the
// definition meets its prototype.
struct R5Link {
    int Link(const R5Svc &next, int kind);
};
int R5Link::Link(const R5Svc &, int) { return 0; }

// With one argument of unknown type, an overload the known one cannot bind
// (a class object passed where a pointer is taken) drops out, so the chained
// call keeps its receiver.
class R5Output {
public:
    void Commit() {}
};
class R5Profile {};
class R5Producer {};
template <class T> class R5Holder {
public:
    T *operator->() const;
};
class R5Manager {
public:
    R5Output *Create(R5Profile &profile, R5Producer &producer);
    int Create(R5Profile &profile, R5Holder<R5Output> *out);
};
void r5_viable(R5Manager *m, R5Producer producer)
{
    auto out = m->Create(r5_profiles(), producer);
    out->Commit();
}
