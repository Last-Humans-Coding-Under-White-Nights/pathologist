void consume(char *value) {}
char *identity(char *value) { return value; }
void unicode(char *p) {
    char *q;
    const char *s = "é"; q /*keep assignment*/ = p;
    const char *wide = "中🙂"; consume /*keep call*/ ( q );
    const char *multi = "é"; q /*keep return*/ = identity /*keep callee*/ ( p );
    const char *chars = "é"; q /*跨🙂*/ =
        p;
}
