char *a;
char *b;

char *returns_a(void) { return a; }
char *fa(void) { return returns_a(); }
char *fb(void) { return b; }
char *fc(void) { return a; }

void indirect_returns(void) {
    char *(*fp)(void);
    fp = fa;
    fp = fb;
    fp = fc;
    char *out = fp();
}
