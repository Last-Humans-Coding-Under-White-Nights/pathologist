// Same-arity overload sets once prototypes keep their parameter types
// (review round 4).
class R4Foo {
public:
    void Go() {}
};
class R4Bar {
public:
    void Go() {}
};

// R4-1: a call bound to the undefined `Get(int)` is not redirected to the
// `Get(long)` definition.
struct R4Sel {
    static R4Foo *Get(int key);
    static R4Bar *Get(long key);
};
R4Bar *R4Sel::Get(long key) { return nullptr; }
void r4_redirect() { R4Sel::Get(1); }

// R4-2: an argument of unknown type keeps both overloads.
void r4_unknown() { R4Sel::Get(r4_undeclared()); }

// R4-3: an out-of-tree namespace's constant is no enumerator.
void R4Take(int value);
void R4Take(R4Foo *foo);
void r4_constant() { R4Take(ext::kFooPtr); }

// R4-4: `Set(int)` and `Set(R4Foo)` are two prototypes, whichever is defined.
struct R4Setter {
    static void Set(int value);
    static void Set(R4Foo foo);
};
void R4Setter::Set(R4Foo foo) {}
void r4_set_int() { R4Setter::Set(5); }

// R4-5: two same-arity substituting overloads; ranking picks one, and its
// own return substitutes.
template <class T> class R4Ptr {
public:
    T *operator->() const;
};
template <class T> class R4Box {
public:
    T *Get(int key);
    R4Ptr<T> Get(const char *key);
};
void r4_substitute(R4Box<R4Foo> &b) { b.Get(1)->Go(); }
