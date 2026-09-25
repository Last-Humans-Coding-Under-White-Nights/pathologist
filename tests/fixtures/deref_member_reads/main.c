// Issue #151: `*x` reads the value `x` holds, not its root variable's.
// See docs/ANALYSIS.md, "Dereferenced operands".
int g_x;
int *g_p = &g_x;
int **g_pp = &g_p;

struct Holder { int **pp; };
struct Holder g_h = { &g_p };

struct Outer { struct Holder *inner; };
struct Outer g_o = { &g_h };

// Load(Load(GEP(h, pp))): the member's value, then through it.
void read_member(struct Holder *h) { int *member_y = *h->pp; }

// Two loads, not one: `**ppp` reads what `*ppp` points to.
void read_twice(int ***ppp) { int *twice_y = **ppp; }

// A dereferenced member as a path root: `(*o->inner).pp` is `o->inner->pp`.
void read_deref_root(struct Outer *o) { int **root_y = (*o->inner).pp; }

// The arrow twin of `read_deref_root`: `(*o->inner).pp` and `o->inner->pp`
// are the same expression, so they must lower (and resolve) the same way.
void read_arrow_root(struct Outer *o) { int **arrow_y = o->inner->pp; }

// A plain `*p` is unchanged: one Load, no temporary.
void read_plain(int **p) { int *plain_y = *p; }

// The store twin: `*h->pp = v` stores through the member's value, into
// g_slot, not into the holder g_w. A distinct struct (not Holder) keeps
// this write out of Holder's field summary, so the reads above stay exact.
struct Slot { int **pp; };
int g_y;
int *g_q = &g_y;
int *g_slot;
struct Slot g_w = { &g_slot };
void write_member(struct Slot *h, int *v) { *h->pp = v; }

// Value positions: a dereferenced member passed, returned or stored is the
// member's value, not the holder. `*p` passed is the value `p` points to.
void take_member(int *q) {}
void pass_member(struct Holder *h) { take_member(*h->pp); }
void take_plain(int *q) {}
void pass_plain(int **p) { take_plain(*p); }
int *return_member(struct Holder *h) { return *h->pp; }
int *g_out;
void store_member(struct Holder *h, int **out) { *out = *h->pp; }
struct Box { int *box_f; };
struct Box g_box;
void field_member(struct Holder *h, struct Box *b) { b->box_f = *h->pp; }

// An element of a member table: `*t->tcells[1]` reads through the element
// (the table's cell's value), `*g_wt.wtcells[0] = v` stores through it.
// Nested array initializers are not lowered, so `drive` fills the tables.
struct Table { int **tcells[2]; };
struct Table g_t;
void read_subscript(struct Table *t) { int *sub_y = *t->tcells[1]; }
struct WTable { int **wtcells[1]; };
int *g_sub_slot;
struct WTable g_wt;
void write_subscript(int *v) { *g_wt.wtcells[0] = v; }

// A store of no pointer value through a member loads nothing.
struct Counter { int *count; };
void zero_count(struct Counter *c) { *c->count = 0; }

// A function pointer is read in place: `*fp` is `fp`, so an argument loads
// nothing through it.
void take_fn(void (*f)(void)) {}
void pass_fn(void (*fp)(void)) { take_fn(*fp); }
struct Ops { void (*op)(void); };
void pass_member_fn(struct Ops *o) { take_fn(*o->op); }

// A pointer to a function pointer is no function pointer: `*pfp` loads the
// function pointer `pfp` points to, read or passed.
void fnp_target(void) {}
void (*g_fnp)(void) = fnp_target;
void read_fn_ptr_ptr(void (**pfp)(void)) { void (*fnp_got)(void) = *pfp; }
void take_fnp(void (*fnp_passed)(void)) {}
void pass_fn_ptr_ptr(void (**pfp)(void)) { take_fnp(*pfp); }

// A store through no pointer the lhs resolves evaluates no rhs either; C
// has no call root to decompose `get_h()->pp` from.
void store_offset(struct Holder *h, int **out) { *(out + 1) = *h->pp; }
struct Holder *get_h(void) { return &g_h; }
void store_call_root(struct Holder *h) { *get_h()->pp = *h->pp; }

// A value position whose operand has no value to compute (an array element,
// a cast) emits nothing, never a load one level short through the root.
int **g_earr[1];
int *return_elem(void) { return *g_earr[0]; }
void store_elem(int **out) { *out = *g_earr[0]; }
int *return_cast(void *p) { return *(int **)p; }

// `*&g_h` is `g_h`: `(*&g_h).pp` reads as `g_h.pp` does.
void read_addr_root(void) { int **addr_y = (*&g_h).pp; int **addr_twin_y = g_h.pp; }

// `(*pp)->ox` reads `ox` off the object `*pp` points to (the out-parameter
// idiom), and so does `(*h->opp)->ox`.
struct OutObj { int *ox; };
struct OutObj g_oobj = { &g_x };
struct OutObj *g_oobj_p = &g_oobj;
void read_out(struct OutObj **pp) { int *out_y = (*pp)->ox; }
struct OutHolder { struct OutObj **opp; };
struct OutHolder g_ohold = { &g_oobj_p };
void read_out_member(struct OutHolder *h) { int *out_member_y = (*h->opp)->ox; }

// An array member's value is its cell: `*h->arr` reads `h->arr[0]`, and
// `*w->warr = v` stores into the element. Distinct structs keep the write
// apart.
struct ArrH { int *arr[2]; };
struct ArrH g_arrh;
void read_array_member(struct ArrH *h) { int *arr_y = *h->arr; int *arr_twin_y = h->arr[0]; }
struct ArrW { int *warr[2]; };
struct ArrW g_arrw;
void write_array_member(struct ArrW *w, int *v) { *w->warr = v; }

// `(*h->objs)->ox` takes the array's first element, a pointer, then its
// arrow. `h->sarr->cb()` calls through the first element of an array of
// structs.
struct ObjsH { struct OutObj *objs[2]; };
struct ObjsH g_objs;
void read_objs(struct ObjsH *h) { int *objs_y = (*h->objs)->ox; }
struct CbS { void (*cb)(void); };
void cb_target(void) {}
struct CbH { struct CbS sarr[2]; };
struct CbH g_cbh;
void set_sarr(struct CbH *h) { h->sarr->cb = cb_target; }
void call_sarr(struct CbH *h) { h->sarr->cb(); }

// A function designator dereferences to itself: `*fn_designated` is
// `fn_designated`, read into an initializer or assigned.
void fn_designated(void) {}
void (*g_desig)(void);
void read_fn_designator(void) {
    void (*desig_got)(void) = *fn_designated;
    g_desig = *fn_designated;
}

// A pointer to a function-pointer member is loaded through: `*h->pcb` is the
// function pointer `h->pcb` points to.
void pcb_target(void) {}
void (*g_pcb_slot)(void) = pcb_target;
struct PcbH { void (**pcb)(void); };
struct PcbH g_pcbh = { &g_pcb_slot };
void call_pcb(struct PcbH *h) {
    void (*cb)(void) = *h->pcb;
    cb();
}

void drive(void) {
    read_member(&g_h);
    call_pcb(&g_pcbh);
    g_objs.objs[0] = &g_oobj;
    read_objs(&g_objs);
    set_sarr(&g_cbh);
    call_sarr(&g_cbh);
    g_arrh.arr[0] = g_p;
    read_array_member(&g_arrh);
    write_array_member(&g_arrw, g_q);
    int *warr_y = g_arrw.warr[0];
    read_out(&g_oobj_p);
    read_out_member(&g_ohold);
    read_twice(&g_pp);
    read_deref_root(&g_o);
    read_arrow_root(&g_o);
    read_plain(&g_p);
    write_member(&g_w, g_q);
    pass_member(&g_h);
    pass_plain(&g_p);
    int *ret_y = return_member(&g_h);
    store_member(&g_h, &g_out);
    field_member(&g_h, &g_box);
    int *box_y = g_box.box_f;
    g_t.tcells[1] = &g_p;
    read_subscript(&g_t);
    g_wt.wtcells[0] = &g_sub_slot;
    write_subscript(g_q);
    read_fn_ptr_ptr(&g_fnp);
    pass_fn_ptr_ptr(&g_fnp);
}
