struct Holder { char **pp; };
void write(struct Holder *h, char *value) {
    *h->pp = value;
}
