/* Issue #127 part C: a variable's value and its memory cell are one object. */
int g;
int h;
void *G1;   /* written through &G1, read directly */
void *G2;   /* written directly, read through &G2 */
void *loc_ident, *glob_ident, *rev_local, *rev_glob;

static void put_g(void **out) { *out = &g; }

void cell_to_node_local(void)  { void *q; void **t = &q; put_g(t); loc_ident = q; }   /* R9  */
void cell_to_node_global(void) { void **t = &G1; put_g(t); glob_ident = G1; }         /* R10 */
void node_to_cell_local(void)  { void *r; void **t = &r; r = &h; rev_local = *t; }    /* R11 */
void node_to_cell_global(void) { void **t = &G2; G2 = &h; rev_glob = *t; }            /* R12 */

typedef void (*cb_t)(void);
static void handler(void) {}
static void get_cb(cb_t *out) { *out = handler; }
void run_cb(void) { cb_t cb; get_cb(&cb); cb(); }                                     /* R13 */

/* A store through a copy of G3's value writes what G3 points to (`slot`),
 * never G3 itself. */
int k;
int slot;
void *G3 = &slot;
void *seed_read;
void seed_is_not_address(void) { void **p = G3; *p = &k; seed_read = G3; }

/* Review of #127: an unrelated `&G4` must not turn the self-location seed
 * into a real address. `p = G4` copies G4's value (&slot), so `*p = &k`
 * writes slot, never G4. */
void *G4;
void **g4_addr;
void *g4_read;
void g4_take_address(void) { G4 = &slot; g4_addr = &G4; }
void g4_store_through_value(void) { void **p = G4; *p = &k; g4_read = G4; }

/* Review of #127: a cell created mid-solve (`return &v` behind an indirect
 * call) carries its declared type's slot guard like any other: an `int *`
 * cell never admits a function value. */
typedef int **(*cell_getter_t)(void);
/* Returning a local's address is deliberate: it is the shape that makes the
 * solver create a variable cell mid-solve (-Wreturn-stack-address is moot for
 * a static analysis fixture). */
static int **give_cell(void) { int *made_mid_solve; return &made_mid_solve; }
static cell_getter_t pick_cell = give_cell;
static void two_args(int a, int b) { (void)a; (void)b; }
void mid_solve_guard(void)
{
    int **c = pick_cell();
    *c = (int *)two_args;
    void (*f)(int, int) = (void (*)(int, int))*c;
    f(1, 2);
}

/* Review of #127: `*p = fn` naming a function defined after the use site is
 * deferred; its temporary still belongs to `store_later_fn`. */
/* No prototype on purpose: invalid C, but lowering must stay tolerant of it
 * (and of a C++ member defined later in its class), deferring the name. */
static void store_later_fn(cb_t *p) { *p = later_fn; }
static void later_fn(void) {}
void run_later_fn(void) { cb_t lcb; store_later_fn(&lcb); lcb(); }

/* Review of #127: a `void *` cell is untyped storage and accepts a function
 * value (dlsym-style `GetSymbol(name, void **out)`). */
static void sym_handler(void) {}
static void get_sym(void **out) { *out = (void *)sym_handler; }
void run_sym(void) { void *sym; get_sym(&sym); cb_t f = (cb_t)sym; f(); }

/* Review of #127: arrays of pointers are arrays — their name denotes their
 * storage — whatever declarator spells them. */
static int *ptr_table[4];
static void put_g_int(int **t) { *t = &g; }
int *table_read;
void run_ptr_table(void) { put_g_int(ptr_table); table_read = *ptr_table; }

static void (*handler_table[4])(void);
static void table_handler(void) {}
static void install(cb_t *tbl) { *tbl = table_handler; }
void run_handler_table(void) { install(handler_table); handler_table[0](); }

/* A pointer to a function returning a pointer, not a pointer to a pointer. */
int *(*int_getter)(void);

/* Review of #127: `return &cache` behind an indirect call makes `cache`
 * address-taken mid-solve, after its value was already known. */
static cb_t cache;
static cb_t *get_cache(void) { return &cache; }
static cb_t *(*cache_getter)(void) = get_cache;
static void cache_handler(void) {}
void init_cache(void) { cache = cache_handler; }
void use_cache(void) { cb_t *pc = cache_getter(); cb_t cf = *pc; cf(); }

/* Review of #127: storing a pointer's *value* (`*out = G5`) hands out what
 * G5 points to, never G5's address, even once `&G5` is taken elsewhere. */
void *G5 = &slot;
void **g5_addr;
void *g5_read;
void g5_take_address(void) { g5_addr = &G5; }
static void g5_get(void **out) { *out = G5; }
void g5_caller(void) { void *p5; g5_get(&p5); *(void **)p5 = &k; }
void g5_reader(void) { g5_read = G5; }

/* ...while storing its *address* (`*out = &G6`) still hands out G6. */
void *G6 = &slot;
void *g6_seen;
static void give_g6(void ***out) { *out = &G6; }
void run_g6(void) { void **pp6; give_g6(&pp6); g6_seen = *pp6; }

/* Review of #127 (round 4). A `void *` table is untyped storage too. */
static void void_table_handler(void) {}
static void *void_table[4];
static void fill_void_table(void **t) { *t = (void *)void_table_handler; }
void run_void_table(void) { fill_void_table(void_table); cb_t vf = (cb_t)void_table[0]; vf(); }

/* A cast member address stores the member, not the whole object. */
struct two_ptrs { void *a; void *b; };
struct two_ptrs tp;
void *member_out;
void store_cast_member(void) { void **mp = &member_out; *mp = (void *)&tp.b; }

/* A cast member address is still a member address for function models. */
void *memcpy(void *dst, const void *src, unsigned long n);
void *copy_src;
void copy_cast_member(void) { memcpy((void *)&tp.a, &copy_src, sizeof copy_src); }

/* A dispatch table that is a struct field. */
struct drv { cb_t ops[2]; };
static struct drv the_drv;
static void op_handler(void) {}
void setup_ops(void) { the_drv.ops[1] = op_handler; }
void run_field_table(void) { the_drv.ops[1](); }
void run_ptr_field_table(void) { struct drv *pd = &the_drv; pd->ops[1](); }

/* A callback argument under a cast. */
static void reg_target(void) {}
static void reg_cb(cb_t c) { c(); }
void do_reg(void) { reg_cb((cb_t)reg_target); }

/* Review of #127: reading a field array's element reads the field, whatever
 * the expression around it. */
struct cb_ops { cb_t callbacks[2]; };
static struct cb_ops cbo, cbo_copy;
static void elem_handler(void) {}
void set_elem(void) { cbo.callbacks[0] = elem_handler; }
void read_elem(void) { cb_t ef = cbo.callbacks[0]; ef(); }
void assign_elem(void) { cb_t eg; eg = cbo.callbacks[0]; eg(); }
void copy_elem(void) { cbo_copy.callbacks[1] = cbo.callbacks[0]; }
void call_copy(void) { cbo_copy.callbacks[1](); }
void pass_elem(void) { reg_cb(cbo.callbacks[0]); }

/* Multi-dimensional dispatch tables, plain and in a struct. */
static cb_t matrix[2][2];
static void matrix_handler(void) {}
static void fill_matrix(cb_t *m) { *m = matrix_handler; }
void run_matrix(void) { fill_matrix(matrix[0]); matrix[1][1](); }
struct machine { cb_t states[2][2]; };
static struct machine mach;
static void state_handler(void) {}
void set_state(void) { mach.states[0][1] = state_handler; }
void run_state(void) { mach.states[1][0](); }

/* ...and so does an element argument under parentheses or a cast. */
static void invoke_paren(cb_t c) { c(); }
static void invoke_cast(cb_t c) { c(); }
void pass_elem_wrapped(void) { invoke_paren((cbo.callbacks[0])); invoke_cast((cb_t)cbo.callbacks[0]); }
void call_cast_elem(void) { ((cb_t)cbo.callbacks[0])(); }
