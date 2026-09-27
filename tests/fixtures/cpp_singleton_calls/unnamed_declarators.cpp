// Unnamed declarators and unresolved classes.
void r9_handler(int v) {}

// R9-1: unnamed function-pointer and array parameters meet their prototypes.
class R9Svc {
public:
    void E(void (*)(int), int);
    void F(const char *const[]);
    void Set();
};
void R9Svc::E(void (*cb)(int), int v) { cb(v); }
void R9Svc::F(const char *const names[]) {}
void R9Svc::Set() { E(r9_handler, 1); }

// R9-2: a reference to a class the unit cannot see (`R9Out` is declared
// nowhere) against an `int` overload, called with an object of that class.
struct R9Setter {
    void Set(const R9Out &f);
    void Set(int v);
};
void r9_unresolved(R9Setter *s, R9Out cfg) { s->Set(cfg); }
