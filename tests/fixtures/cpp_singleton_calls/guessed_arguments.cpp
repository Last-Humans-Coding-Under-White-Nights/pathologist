// Guessed argument types never pick a single overload.
class R7Config {};

// R7-1: an out-of-tree class constant spelled like an enumerator.
void R7Log(int level);
void R7Log(const char *tag);
void r7_constant() { R7Log(Consts::DEFAULT_TAG); }

// R7-2: an argument of a class the unit cannot resolve lowers as `int`.
struct R7Setter {
    void Set(int v);
    void Set(const R7Config &cfg);
};
void r7_standin(R7Setter *s, R7Unresolved cfg) { s->Set(cfg); }

// R7-4: classes of one name in unrelated namespaces, one of them undeclared.
namespace r7a {
struct Config {};
}
struct R7Apply {
    static void Apply(const r7a::Config &c);
    static void Apply(int v);
};
void r7_other_namespace(r7b::Config c) { R7Apply::Apply(c); }
