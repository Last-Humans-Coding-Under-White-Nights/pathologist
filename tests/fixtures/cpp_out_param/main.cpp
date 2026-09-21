// Issue #127 review: C++ out-parameter idioms.
struct X {
    int v;
};

X global;
X *seen;

static void save(X *p) { seen = p; }

// `&r` for a reference `r` is the referent's address.
void caller() {
    X &r = global;
    save(&r); // `seen` must point to `global`, not to a cell for `r`
}

// `p = &r` for a reference `r` is the referent's address too.
X *seen_through_ref;
void assign_through_reference() {
    X &r2 = global;
    X *p2 = &r2;
    seen_through_ref = p2;
}

// ...and so is `&a` for an `auto &a` binding.
X &GetInst() {
    static X inst;
    return inst;
}
X *seen_auto_ref;
static void save_auto(X *p) { seen_auto_ref = p; }
void caller_auto_ref() {
    auto &a = GetInst();
    save_auto(&a);
}

// A callback returned through an out-parameter as `&fn` or through a cast.
typedef void (*cb_t)();
static void by_address() {}
static void by_cast() {}
static void get_by_address(cb_t *out) { *out = &by_address; }
static void get_by_cast(cb_t *out) { *out = (cb_t)by_cast; }

void run_by_address() {
    cb_t cb;
    get_by_address(&cb);
    cb();
}

void run_by_cast() {
    cb_t cb;
    get_by_cast(&cb);
    cb();
}

// Qualified function designators: a namespace function and a static member.
namespace ns {
void ns_cb() {}
}
struct Cls {
    static void handler();
};
void Cls::handler() {}
static void get_ns(cb_t *out) { *out = &ns::ns_cb; }
static void get_cls(cb_t *out) { *out = Cls::handler; }

void run_ns() {
    cb_t cb;
    get_ns(&cb);
    cb();
}

void run_cls() {
    cb_t cb;
    get_cls(&cb);
    cb();
}

// A C++ named cast around `&x` still passes x's address.
X *seen_cast;
static void save_cast(X *p) { seen_cast = p; }
void caller_cast() {
    static X cast_target;
    save_cast(static_cast<X *>(&cast_target));
}

// C++ named casts in assignments, returns and arguments.
X *cast_src_ptr = &global;
X *assigned_cast;
void assign_named_cast() { assigned_cast = static_cast<X *>(cast_src_ptr); }
void *ret_cast() { return static_cast<void *>(cast_src_ptr); }
void *ret_seen;
void use_ret_cast() { ret_seen = ret_cast(); }
X *arg_seen;
static void take_x(X *p) { arg_seen = p; }
void pass_named_cast() { take_x(static_cast<X *>(cast_src_ptr)); }

// `return &r` for a reference `r` is the referent's address too.
X *addr_of_ref_return() {
    X &r3 = global;
    return &r3;
}
X *seen_ref_return;
void use_ref_return() { seen_ref_return = addr_of_ref_return(); }
