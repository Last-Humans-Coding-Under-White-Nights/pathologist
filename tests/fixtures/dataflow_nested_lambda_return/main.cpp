typedef char *(*Fn)(char *);
char *id(char *p) { return p; }
char *invoke(Fn fn, char *p) { return fn(p); }
void run(char *input) {
    char *out;
    out = invoke([](char *p) {
        char *inner;
        inner = id(p);
        return inner;
    }, input);
}
