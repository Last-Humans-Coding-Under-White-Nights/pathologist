struct S { char *field; };
void block(struct S *s, char *p) {
    s->field /* = note */ = p;
}
void delimiters(struct S *s, char *p) {
    s->field /* [ " */ = p;
}
void line(struct S *s, char *p) {
    s->field // = [ "
        = p;
}
