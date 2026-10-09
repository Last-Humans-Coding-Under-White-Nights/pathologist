struct S { char **slots[1001]; };
int index(const char *text) { return 0; }
void digits(S *s, char *p) {
    *s->slots[1'000] = p;
}
void raw(S *s, char *p) {
    *s->slots[index(R"tag(" ] } = /* ')tag")] = p;
}
void multiline(S *s, char *p) {
    *s->slots[index(u8R"tag(" ] =
\
/* ')tag")] = p;
}
