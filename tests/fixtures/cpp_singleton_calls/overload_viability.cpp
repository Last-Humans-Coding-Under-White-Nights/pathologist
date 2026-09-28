// Overload viability and registration.
class R6Foo {
public:
    void Go() {}
};
template <class T> class R6Sptr {
public:
    R6Sptr(T *raw);
    T *operator->() const;
};

// R6-1: a raw pointer converts to `R6Sptr<R6Foo>`; with an unknown argument
// beside it, that overload stays a candidate.
struct R6Conv {
    static void Set(R6Sptr<R6Foo> f, int x);
    static void Set(const char *n, int x);
};
void r6_convert(R6Foo *raw) { R6Conv::Set(raw, r6_unknown()); }

// R6-2: a prototype after an inline definition of another overload.
struct R6Inline {
    void Set(int v) { v_ = v; }
    void Set(R6Foo f);
    int v_;
};
void R6Inline::Set(R6Foo f) {}
void r6_inline(R6Inline *a, R6Foo f) { a->Set(f); }

// R6-6: unnamed `int *const` and `int **` parameters.
struct R6Ptrs {
    static int Get(int *);
    static int Get(int **);
};
int R6Ptrs::Get(int *const) { return 1; }
int R6Ptrs::Get(int **) { return 2; }
