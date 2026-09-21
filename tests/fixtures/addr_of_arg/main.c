/* Issue #127 part B: `f(&x)` passes x's address, not x's value. */
int g;
int **seen;
int *taken;

static void get_buf(int **out) { *out = &g; }
static void peek(int **out) { seen = out; }

void caller(void) {
    int *q;
    get_buf(&q);      /* R2: formal `out` must point to q.            */
    taken = q;        /* R8: needs Part C (variable-cell sync)        */
}

void caller_peek(void) {
    int *r;
    peek(&r);         /* R5 */
}

int x;

void caller_init(void) {
    int *s = &x;      /* s already points somewhere...               */
    peek(&s);         /* R7: ...so `seen` must get s's address, not x */
}
