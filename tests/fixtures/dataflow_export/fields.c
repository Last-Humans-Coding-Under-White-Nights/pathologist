struct S { char *f; };
struct Outer { struct S inner; };

void initializers(char *v) {
    struct S positional = { v };
    struct S designated = { .f = v };
    struct Outer nested = { .inner.f = v };
}
