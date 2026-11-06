using Callback = void (*)();
void alpha();
void beta();
void unrelated();
Callback touch();
struct LvalueOps { Callback cb; };
struct LvalueNoise { int pad; Callback cb; };
void conditional_field_root(int c) {
    LvalueOps a{alpha}, b{beta};
    LvalueNoise noise{0, unrelated};
    (c ? &a : &b)->cb();
}
void conditional_field_control(int c) {
    LvalueOps a{alpha}, b{beta};
    LvalueOps *p = c ? &a : &b;
    p->cb();
}
void compound_field_root(int c) {
    LvalueOps a{alpha}, b{beta};
    (touch(), c ? &a : &b)->cb();
}
void conditional_object_deref(int c) {
    LvalueOps a{alpha}, b{beta};
    (*(c ? &a : &b)).cb();
}
LvalueOps root_alpha{alpha}, root_beta{beta};
LvalueOps *get_root_alpha() { return &root_alpha; }
LvalueOps *get_root_beta() { return &root_beta; }
void conditional_field_call_roots(int c) {
    (c ? get_root_alpha() : get_root_beta())->cb();
}
void conditional_deref_read(int c) {
    Callback a = alpha, b = beta;
    Callback *pa = &a, *pb = &b;
    Callback f = *(c ? pa : pb);
    f();
}
void conditional_deref_control(int c) {
    Callback a = alpha, b = beta;
    Callback *p = c ? &a : &b;
    Callback f = *p;
    f();
}
void conditional_deref_store(int c) {
    Callback a = nullptr, b = nullptr;
    *(c ? &a : &b) = alpha;
    a(); b();
}
void consume_compound_deref(Callback f) { f(); }
void conditional_deref_argument(int c) {
    Callback a = alpha, b = beta;
    consume_compound_deref(*(c ? &a : &b));
}
void conditional_function_address(int c) {
    Callback f = &(c ? alpha : beta);
    f();
}
void conditional_array_address(int c) {
    Callback a[2] = {alpha, beta};
    Callback *p = &(c ? a[0] : a[1]);
    Callback f = *p;
    f();
}
void consume_comma_address(Callback *p) { (*p)(); }
void standalone_comma_address() {
    Callback a = alpha;
    consume_comma_address(&(touch(), a));
}
void consume_assignment_address(Callback *p) { (*p)(); }
void standalone_assignment_address() {
    Callback a = alpha, p = nullptr;
    consume_assignment_address(&(p = a));
}
void consume_standalone_comma(Callback f) { f(); }
void standalone_comma_argument() { consume_standalone_comma((touch(), alpha)); }
void standalone_comma_callee() { (touch(), alpha)(); }
void conditional_lvalue_assignment(int c) {
    Callback a = nullptr, b = nullptr;
    Callback f = ((c ? a : b) = alpha);
    f(); a(); b();
}
void standalone_conditional_lvalue_assignment(int c) {
    Callback a = nullptr, b = nullptr;
    (c ? a : b) = beta;
    a(); b();
}
struct WrittenOps { Callback cb; };
void conditional_field_assignment(int c) {
    WrittenOps a{nullptr}, b{nullptr};
    (c ? a.cb : b.cb) = alpha;
    a.cb(); b.cb();
}
void comma_lvalue_assignment() {
    Callback a = nullptr;
    (touch(), a) = alpha;
    a();
}
void conditional_field_root_store(int c) {
    WrittenOps a{nullptr}, b{nullptr};
    (c ? &a : &b)->cb = alpha;
    a.cb(); b.cb();
}

Callback reference_a = alpha, reference_b = beta;
Callback &get_reference_a() { return reference_a; }
Callback &get_reference_b() { return reference_b; }
void consume_reference_address(Callback *p) { (*p)(); }
void conditional_reference_call_address(int c) {
    consume_reference_address(&(c ? get_reference_a() : get_reference_b()));
}
Callback &forward_reference_a() { return get_reference_a(); }
void consume_forward_reference(Callback *p) { (*p)(); }
void forwarded_reference_address() { consume_forward_reference(&forward_reference_a()); }
struct ReferenceHolder {
    Callback cb;
    Callback &get() { return cb; }
};
void consume_reference_member(Callback *p) { (*p)(); }
void reference_member_address(int c) {
    ReferenceHolder a{alpha}, b{beta};
    consume_reference_member(&(c ? a.get() : b.get()));
}
Callback empty_reference_a = nullptr, empty_reference_b = nullptr;
Callback &get_empty_reference_a() { return empty_reference_a; }
Callback &get_empty_reference_b() { return empty_reference_b; }
void write_reference_address(int c) {
    Callback *p = &(c ? get_empty_reference_a() : get_empty_reference_b());
    *p = alpha;
    empty_reference_a(); empty_reference_b();
}
using ReferenceGetter = Callback &(*)();
void consume_indirect_reference(Callback *p) { (*p)(); }
void indirect_reference_address(int c) {
    ReferenceGetter getter = c ? get_reference_a : get_reference_b;
    consume_indirect_reference(&getter());
}
Callback &remote_reference();
void consume_remote_reference(Callback *p) { (*p)(); }
void cross_tu_reference_address(int c) {
    consume_remote_reference(&(c ? remote_reference() : get_reference_b()));
}
void ordinary_reference_value() { Callback f = get_reference_a(); f(); }
Callback &dereferenced_reference(Callback *p) { return *p; }
void consume_dereferenced_reference(Callback *p) { (*p)(); }
void dereferenced_reference_address() {
    Callback a = alpha;
    consume_dereferenced_reference(&dereferenced_reference(&a));
}
struct ForwardReferenceHolder {
    Callback cb;
    Callback &get() { return cb; }
    Callback &forward() { return get(); }
};
void consume_forward_member(Callback *p) { (*p)(); }
void forwarded_member_reference_address() {
    ForwardReferenceHolder h{alpha};
    consume_forward_member(&h.forward());
    Callback f = h.forward();
    f();
}
struct ReferenceBase { virtual Callback &get() = 0; };
struct ReferenceAlpha : ReferenceBase { Callback &get() override { return reference_a; } };
struct ReferenceBeta : ReferenceBase { Callback &get() override { return reference_b; } };
void consume_virtual_reference(Callback *p) { (*p)(); }
void virtual_reference_address(ReferenceBase *p) { consume_virtual_reference(&p->get()); }
