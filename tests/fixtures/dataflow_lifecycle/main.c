struct S { char *p; };
char *id(char *value) { return value; }
void flow(struct S *s, char *payload) {
    char *local = payload;
    local = payload;
    s->p = id(payload);
}
