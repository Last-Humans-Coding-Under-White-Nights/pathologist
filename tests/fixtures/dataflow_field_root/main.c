struct S { int *field; };
void f(struct S *s, int *p) {
    s->field = p;
}
