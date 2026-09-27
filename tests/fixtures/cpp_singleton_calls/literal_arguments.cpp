// Ranking never lets a guess choose one overload.
class R8Str {};

// R8-1: a literal `int` against a scalar and a class-reference overload.
struct R8Writer {
    void Write(long v);
    void Write(const R8Str &s);
};
void r8_literal(R8Writer *w) { w->Write(5); }

// R8-3: an enum-shaped constant that is an array of strings.
void R8Run(const char **args);
void R8Run(int flags);
void r8_args() { R8Run(Config::DEFAULT_ARGS); }

// R8-6 / R13-4: a free call's known argument ranks its overloads even beside
// an argument of unknown type.
struct R8X {};
struct R8Y {};
struct R8Make {
    static int Make(R8X x, int n);
    static int Make(R8Y y, long n);
};
void r8_unknown() { R8Make::Make(r8_unknown_value(), 5); }
