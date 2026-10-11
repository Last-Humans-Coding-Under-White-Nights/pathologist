struct Left { void *member; };
struct Right { void *member; };
int payload;
void Alias(void *dst, void *src);
void precise(void) {
    struct Left left;
    struct Right right;
    left.member = &payload;
    Alias((void *)&right.member, (void *)&left.member);
}
void fallback(void) {
    struct Left left;
    void *slots[2];
    left.member = &payload;
    Alias(&slots[1], &left.member);
}
