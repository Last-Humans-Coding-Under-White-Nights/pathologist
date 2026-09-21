/* Issue #127: loads through cells whose declared type is a pointer or an
 * aggregate must return the non-function values stored there. */
int g;
int *direct;
int *viaload;
int *taken_via_var;

struct box { int *ptr; };
struct box bx;
struct box *boxload;

void f(void) {
    int *p;
    int **pp = &p;
    *pp = &g;          /* store through int**: &g reaches p's cell   */
    direct  = &g;      /* control                                    */
    viaload = *pp;     /* R1: load back through int**                */
}

static void get_buf(int **out) { *out = &g; }

void caller_var(void) {
    int *q;
    int **t = &q;
    get_buf(t);         /* R4: out-param passed through a variable    */
    taken_via_var = *t; /* read back through the cell (a Load)        */
}

void aggregate(void) {
    struct box *bp;
    struct box **pbp = &bp;
    *pbp = &bx;        /* store a struct address into a Ptr cell     */
    boxload = *pbp;    /* load it back: boxload -> bx                */
}
