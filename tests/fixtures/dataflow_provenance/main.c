void recur(char *p) {
    char *q = p;
    p = q;
    recur(q);
}

void variant(char *p) {
    char *q;
#ifdef FEATURE
    q = p;
#else
    q = p;
#endif
}
