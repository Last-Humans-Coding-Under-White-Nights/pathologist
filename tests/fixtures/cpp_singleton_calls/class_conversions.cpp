// A class argument converting to a parameter class keeps that overload.
// R17-2: a derived object binds `const Base &` beside a scalar overload.
class R17Base {};
class R17Derived : public R17Base {};
void R17Take(const R17Base &b);
void R17Take(int v);
void r17_derived(R17Derived d) { R17Take(d); }
