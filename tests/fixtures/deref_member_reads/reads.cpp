// Issue #151, C++ mirror of main.c: the same dereferenced reads and write,
// lowered by the C++ grammar. Every name here is `cpp_`-prefixed and no
// struct is shared with main.c, so name lookups stay unique and the field
// summaries stay separate.
int cpp_g_x;
int *cpp_g_p = &cpp_g_x;
int **cpp_g_pp = &cpp_g_p;

struct CppHolder { int **pp; };
CppHolder cpp_g_h = { &cpp_g_p };

struct CppOuter { CppHolder *inner; };
CppOuter cpp_g_o = { &cpp_g_h };

// Load(Load(GEP(h, pp))): the member's value, then through it.
void cpp_read_member(CppHolder *h) { int *cpp_member_y = *h->pp; }

// Two loads, not one: `**ppp` reads what `*ppp` points to.
void cpp_read_twice(int ***ppp) { int *cpp_twice_y = **ppp; }

// A dereferenced member as a path root: `(*o->inner).pp` is `o->inner->pp`.
void cpp_read_deref_root(CppOuter *o) { int **cpp_root_y = (*o->inner).pp; }

// The arrow twin: `(*o->inner).pp` and `o->inner->pp` are the same
// expression, so they must lower (and resolve) the same way.
void cpp_read_arrow_root(CppOuter *o) { int **cpp_arrow_y = o->inner->pp; }

// A plain `*p` is unchanged: one Load, no temporary.
void cpp_read_plain(int **p) { int *cpp_plain_y = *p; }

// The store twin: a distinct struct keeps this write out of CppHolder's
// field summary.
struct CppSlot { int **pp; };
int cpp_g_y;
int *cpp_g_q = &cpp_g_y;
int *cpp_g_slot;
CppSlot cpp_g_w = { &cpp_g_slot };
void cpp_write_member(CppSlot *h, int *v) { *h->pp = v; }

// Value positions: a dereferenced member passed, returned or stored is the
// member's value, not the holder. `*p` passed is the value `p` points to.
void cpp_take_member(int *q) {}
void cpp_pass_member(CppHolder *h) { cpp_take_member(*h->pp); }
void cpp_take_plain(int *q) {}
void cpp_pass_plain(int **p) { cpp_take_plain(*p); }
int *cpp_return_member(CppHolder *h) { return *h->pp; }
int *cpp_g_out;
void cpp_store_member(CppHolder *h, int **out) { *out = *h->pp; }
struct CppBox { int *box_f; };
CppBox cpp_g_box;
void cpp_field_member(CppHolder *h, CppBox *b) { b->box_f = *h->pp; }

// An element of a member table: `*t->tcells[1]` reads through the element
// (the table's cell's value), `*cpp_g_wt.wtcells[0] = v` stores through it.
// Nested array initializers are not lowered, so `cpp_drive` fills the tables.
struct CppTable { int **tcells[2]; };
CppTable cpp_g_t;
void cpp_read_subscript(CppTable *t) { int *cpp_sub_y = *t->tcells[1]; }
struct CppWTable { int **wtcells[1]; };
int *cpp_g_sub_slot;
CppWTable cpp_g_wt;
void cpp_write_subscript(int *v) { *cpp_g_wt.wtcells[0] = v; }

// A store of no pointer value through a member loads nothing.
struct CppCounter { int *count; };
void cpp_zero_count(CppCounter *c) { *c->count = 0; }

// A member body's bare instance field is read through `this`: `*m_rpp`
// reads through the member's value, `*m_wpp = q` stores through it.
int *cpp_g_this_slot;
struct CppReader {
    int **m_rpp;
    void read() { int *cpp_this_y = *m_rpp; }
};
CppReader cpp_g_reader = { &cpp_g_p };
struct CppWriter {
    int **m_wpp;
    void write(int *q) { *m_wpp = q; }
};
CppWriter cpp_g_writer = { &cpp_g_this_slot };

// A dereference passed to a reference parameter passes the referent's
// address, which the reference holds; to a pointer parameter it passes the
// value read.
struct CppObj { void (*cb)(); };
void cpp_visit_ref(CppObj &o);
void cpp_call_ref(CppObj *p) { cpp_visit_ref(*p); }
struct CppVisitor { void visit(CppObj &o) {} };
void cpp_call_member_ref(CppVisitor *v, CppObj *p) { v->visit(*p); }
void cpp_visit_ptr(CppObj *o) {}
void cpp_call_ptr(CppObj **pp) { cpp_visit_ptr(*pp); }
// A member defined out of line after the call: its in-class prototype
// declares the reference, so the call passes the address.
struct CppLateVisitor { void visit(CppObj &o); };
void cpp_call_out_of_line(CppLateVisitor *v, CppObj *p) { v->visit(*p); }
void CppLateVisitor::visit(CppObj &o) {}

// Reference-ness comes from the declaration: a member declared in its
// class and defined out of line after the call takes a pointer, so the value
// read is passed; an unnamed reference parameter takes the address.
struct CppTaker { void take(int *q); };
void cpp_call_declared(CppTaker *v, int **pp) { v->take(*pp); }
void CppTaker::take(int *q) {}
void cpp_visit_unnamed(CppObj &);
void cpp_call_unnamed(CppObj *p) { cpp_visit_unnamed(*p); }

// A member body's bare instance field, dereferenced as an argument, passes
// the value read through it.
void cpp_take_this(int *q) {}
struct CppPasser {
    int **m_ppp;
    void pass() { cpp_take_this(*m_ppp); }
};

// One `*pp` passed to an override set is loaded once, for every target.
struct CppBase { virtual void vtake(int *q) {} };
struct CppDerived : CppBase { void vtake(int *q) override {} };
void cpp_call_virtual(CppBase *b, int **pp) { b->vtake(*pp); }

// A dereferenced root whose own path does not decompose (no layout
// declares `absent`) reads nothing off the holder.
void cpp_read_unresolved_root(CppOuter *o) { int **cpp_unres_y = (*o->absent).pp; }

// A reference to a pointer is bound to the pointer's value: `*r` is one
// load, and `&r` is `r`'s own address.
void cpp_read_ref_ptr() {
    int **&cpp_ref_r = cpp_g_pp;
    int *cpp_ref_z = *cpp_ref_r;
    int ***cpp_ref_a = &cpp_ref_r;
}

// An explicit `this->m_epp` reads and writes as the bare member does.
int *cpp_g_ethis_slot;
struct CppExplicitReader {
    int **m_epp;
    void read() { int *cpp_ethis_y = *this->m_epp; }
};
CppExplicitReader cpp_g_ereader = { &cpp_g_p };
struct CppExplicitWriter {
    int **m_epp;
    void write(int *q) { *this->m_epp = q; }
};
CppExplicitWriter cpp_g_ewriter = { &cpp_g_ethis_slot };

// A member a base declares resolves through the base's layout, so a store
// through `this->bpp` stores through `bpp`.
struct CppBaseHolder { int **bpp; };
struct CppDerivedHolder : CppBaseHolder {
    void store(CppHolder *h) { *this->bpp = *h->pp; }
};

// `*&m_ap` dereferences an address, which no value computed here
// resolves: the store evaluates nothing.
struct CppAddrOfMember {
    int *m_ap;
    void store(CppHolder *h) { *&m_ap = *h->pp; }
};

// A function returning a reference returns the referent's address: `*p`
// returns `p`'s value.
CppHolder *cpp_g_hp = &cpp_g_h;
CppHolder &cpp_inst() { return *cpp_g_hp; }

// Positions past a variadic list and a by-value pack's elements take the
// value read; a reference pack's elements take the address. A lambda is
// called through its closure variable, so its call keeps the address.
void cpp_vtake(...) {}
void cpp_call_variadic(int **pp) { cpp_vtake(*pp); }
template <class... A> void cpp_ptake(A... a) {}
void cpp_call_pack(int **pp) { cpp_ptake(*pp); }
template <class... A> void cpp_rtake(A &...a) {}
void cpp_call_ref_pack(CppObj *p) { cpp_rtake(*p); }
void cpp_call_lambda(int **pp) {
    auto l = [](int *q) {};
    l(*pp);
}

// `(*this).m_dpp` is `this->m_dpp`.
struct CppDerefThis {
    int **m_dpp;
    void read() {
        int *cpp_dthis_y = *(*this).m_dpp;
        int **cpp_dthis_z = (*this).m_dpp;
        int **cpp_dthis_w = this->m_dpp;
    }
};
CppDerefThis cpp_g_dthis = { &cpp_g_p };

// A store through a longer path rooted at an instance field, bare
// (`m_in->pp`) or through `this` (`this->a->pp`), stores through the
// member's value, as a read would. Distinct structs keep the writes apart.
int *cpp_g_min_slot;
struct CppMIn { int **pp; };
CppMIn cpp_g_min = { &cpp_g_min_slot };
struct CppMStore {
    CppMIn *m_in;
    void w(int *q) { *m_in->pp = q; }
};
CppMStore cpp_g_mstore = { &cpp_g_min };
int *cpp_g_tin_slot;
struct CppTIn { int **pp; };
CppTIn cpp_g_tin = { &cpp_g_tin_slot };
struct CppTStore {
    CppTIn *a;
    void w(int *q) { *this->a->pp = q; }
};
CppTStore cpp_g_tstore = { &cpp_g_tin };

// A reference returned through a trailing return type, a lambda's included,
// is the referent's address too; `decltype(auto)` returns by value.
auto cpp_inst_trailing() -> CppHolder & { return *cpp_g_hp; }
void cpp_make_lambda() {
    auto l = []() -> CppHolder & { return *cpp_g_hp; };
}
decltype(auto) cpp_inst_value() { return *cpp_g_hp; }

// A pack and a trailing `...` are one variadic tail: `vpack` declares no
// parameter. A lambda's parameters are declared as a function's are.
struct CppVariadic {
    template <class... A> void vpack(A... a, ...);
};
void cpp_lambda_params() {
    auto l = [](int *q, CppObj &o) {};
}

// An indirect call's formal is unknown at the call: `*pp` passes a value
// holding both `pp`'s value and the value read.
void cpp_ind_take(int *q) { int *cpp_ind_q = q; }
void cpp_call_indirect(int **pp) {
    void (*fp)(int *) = cpp_ind_take;
    fp(*pp);
}

// `(*pp)->ox` reads `ox` off the object `*pp` points to (the out-parameter
// idiom), and so does `(*h->opp)->ox`.
struct CppOutObj { int *ox; };
CppOutObj cpp_g_oobj = { &cpp_g_x };
CppOutObj *cpp_g_oobj_p = &cpp_g_oobj;
void cpp_read_out(CppOutObj **pp) { int *cpp_out_y = (*pp)->ox; }
struct CppOutHolder { CppOutObj **opp; };
CppOutHolder cpp_g_ohold = { &cpp_g_oobj_p };
void cpp_read_out_member(CppOutHolder *h) { int *cpp_out_member_y = (*h->opp)->ox; }

// A member outranks a global of its name in a member body, defined in its
// class or out of line.
int *cache;
struct CppCacheOwner {
    int *cache;
    void f() { int **cpp_cache_a = &cache; }
    void g();
};
void CppCacheOwner::g() { int **cpp_cache_b = &cache; }

// `*this` is an operand as any `*p` is: `return *this` from a function
// returning a reference returns `this`; passed, it binds a reference to
// `this` and a by-value parameter to the value read; stored through, it
// stores into the object.
struct CppChain;
void cpp_chain_ref(CppChain &c);
void cpp_chain_val(CppChain c);
struct CppChain {
    int *m_v;
    CppChain &set(int *v) {
        m_v = v;
        return *this;
    }
    void pass() {
        cpp_chain_ref(*this);
        cpp_chain_val(*this);
    }
    void assign(CppChain *o) { *this = *o; }
};
CppChain cpp_g_chain;
void cpp_chain(int *a, int *b) { cpp_g_chain.set(a).set(b); }

// A member call whose result nothing records is no operand value.
struct CppNoFlow { int **Get(); };
CppNoFlow *cpp_make_noflow();
void cpp_read_noflow() { int *cpp_noflow_y = *cpp_make_noflow()->Get(); }
CppNoFlow *cpp_g_noflow;
void cpp_read_noflow_var() { int *cpp_noflow_var_y = *cpp_g_noflow->Get(); }

// A block-scope `using` hides a member of its name, as a local does: the
// address and the read both name `cpp_uns::ucache`.
namespace cpp_uns {
int *ucache;
}
struct CppUsingOwner {
    int *ucache;
    void f() {
        using cpp_uns::ucache;
        int **cpp_using_a = &ucache;
        int *cpp_using_r = ucache;
    }
};

// An array member's value is its cell, as in main.c.
struct CppArrH { int *arr[2]; };
CppArrH cpp_g_arrh;
void cpp_read_array_member(CppArrH *h) {
    int *cpp_arr_y = *h->arr;
    int *cpp_arr_twin_y = h->arr[0];
}
struct CppArrW { int *warr[2]; };
CppArrW cpp_g_arrw;
void cpp_write_array_member(CppArrW *w, int *v) { *w->warr = v; }

// `(*h->objs)->ox` takes the array's first element, a pointer, then its
// arrow.
struct CppObjsH { CppOutObj *objs[2]; };
CppObjsH cpp_g_objs;
void cpp_read_objs(CppObjsH *h) { int *cpp_objs_y = (*h->objs)->ox; }

// A member array of class objects is initialized element by element.
struct CppElem { CppElem(); };
struct CppArrOwner {
    CppElem m_arr[2];
    CppArrOwner() : m_arr{} {}
};

// A function-pointer member a base declares resolves through the base's
// layout: its call's result is recorded.
struct CppGetBase { int **(*get)(); };
struct CppGetDerived : CppGetBase {};
void cpp_read_inherited_get(CppGetDerived *d) { int *cpp_get_y = *d->get(); }

// A dereferenced bare instance field roots its path at `this`, as its
// arrow twin does.
struct CppDerefMember {
    CppHolder *m_in;
    void deref() {
        int **cpp_dm_y = (*m_in).pp;
        int **cpp_dm_twin_y = m_in->pp;
    }
};
CppDerefMember cpp_g_dm = { &cpp_g_h };

// An arrow on an array member reaches its element's class.
struct CppRunElem { void Run(); };
struct CppRunHolder {
    CppRunElem m_items[2];
    void a() { m_items->Run(); }
};
void cpp_run_items(CppRunHolder *h) { h->m_items->Run(); }

// A member array's brace list initializes its elements one by one: an
// empty one default-constructs them, a 2-D one too, and listed elements
// are expressions of their own.
struct CppElemArgs {
    CppElemArgs();
    CppElemArgs(int *a);
};
struct CppArrOwner2 {
    CppElemArgs m_arr[2];
    CppElemArgs m_grid[2][2];
    CppArrOwner2(int *a, int *b) : m_arr{CppElemArgs(a), CppElemArgs(b)}, m_grid{} {}
};

// A member outranks a global of its name for the operand's type too: `*m_gpp`
// loads through `this->m_gpp`, not in place as the global function pointer
// would be read.
void (*m_gpp)(void);
struct CppTypeOwner {
    int **m_gpp;
    void read() { int *cpp_gpp_y = *m_gpp; }
    void read_out_of_line();
};
void CppTypeOwner::read_out_of_line() { int *cpp_gpp_z = *m_gpp; }
CppTypeOwner cpp_g_type_owner = { &cpp_g_p };

// A class-typed member called through its `operator()` records no call
// result, so its dereference is no operand value.
struct CppFunctor { int **operator()(); };
struct CppFunctorHolder { CppFunctor functor; };
void cpp_read_functor(CppFunctorHolder *h) { int *cpp_functor_y = *h->functor(); }

// An array member's value is its cell in every value position: read into
// a variable (bare in a member body or through a path) or returned, it
// holds the array's address, as `&h->varr` does, not what its elements
// hold. A returned pointer member is its value, loaded with its own type.
// (`this` is bound to no object here: the bare reads resolve through the
// member's field summary, as `&varr` does.)
struct CppArrVal {
    int *varr[2];
    int **m_vpp;
    void read_bare() {
        int *(*cpp_arrv_bare_addr)[2] = &varr;
        int **cpp_arrv_bare = varr;
        int **cpp_arrv_bare_assigned;
        cpp_arrv_bare_assigned = varr;
    }
    int **ret_arr() { return varr; }
    int **ret_pp() { return m_vpp; }
};
CppArrVal cpp_g_arrv;
void cpp_read_arr_value(CppArrVal *h) {
    int **cpp_arrv_field = h->varr;
    int **cpp_arrv_assigned;
    cpp_arrv_assigned = h->varr;
    int *(*cpp_arrv_addr)[2] = &h->varr;
}
int **cpp_arrv_get(CppArrVal *h) { return h->varr; }

void cpp_drive() {
    cpp_read_member(&cpp_g_h);
    cpp_g_objs.objs[0] = &cpp_g_oobj;
    cpp_read_objs(&cpp_g_objs);
    cpp_g_arrh.arr[0] = cpp_g_p;
    cpp_read_array_member(&cpp_g_arrh);
    cpp_write_array_member(&cpp_g_arrw, cpp_g_q);
    int *cpp_warr_y = cpp_g_arrw.warr[0];
    cpp_call_indirect(&cpp_g_p);
    cpp_read_out(&cpp_g_oobj_p);
    cpp_read_out_member(&cpp_g_ohold);
    cpp_read_twice(&cpp_g_pp);
    cpp_read_deref_root(&cpp_g_o);
    cpp_read_arrow_root(&cpp_g_o);
    cpp_read_plain(&cpp_g_p);
    cpp_write_member(&cpp_g_w, cpp_g_q);
    cpp_pass_member(&cpp_g_h);
    cpp_pass_plain(&cpp_g_p);
    int *cpp_ret_y = cpp_return_member(&cpp_g_h);
    cpp_store_member(&cpp_g_h, &cpp_g_out);
    cpp_field_member(&cpp_g_h, &cpp_g_box);
    int *cpp_box_y = cpp_g_box.box_f;
    cpp_g_t.tcells[1] = &cpp_g_p;
    cpp_read_subscript(&cpp_g_t);
    cpp_g_wt.wtcells[0] = &cpp_g_sub_slot;
    cpp_write_subscript(cpp_g_q);
    cpp_g_reader.read();
    cpp_g_writer.write(cpp_g_q);
    cpp_read_ref_ptr();
    cpp_g_ereader.read();
    cpp_g_ewriter.write(cpp_g_q);
    cpp_g_dthis.read();
    cpp_g_type_owner.read();
    cpp_g_dm.deref();
    cpp_g_mstore.w(cpp_g_q);
    cpp_g_tstore.w(cpp_g_q);
    cpp_g_arrv.varr[0] = &cpp_g_x;
    cpp_read_arr_value(&cpp_g_arrv);
    cpp_g_arrv.read_bare();
    int **cpp_arrv_ret = cpp_arrv_get(&cpp_g_arrv);
}

// A function designator dereferences to itself, as in main.c.
void cpp_fn_designated(void) {}
void (*cpp_g_desig)(void);
void cpp_read_fn_designator(void) {
    void (*cpp_desig_got)(void) = *cpp_fn_designated;
    cpp_g_desig = *cpp_fn_designated;
}
