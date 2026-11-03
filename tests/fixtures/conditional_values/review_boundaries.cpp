using BoundaryCallback = void (*)();
void alpha(); void beta(); void unrelated();
using HiddenReference = BoundaryCallback&;
namespace BoundaryTypes {
struct HiddenReference {
    HiddenReference(BoundaryCallback f) { f(); }
    void run() {}
};
void constructor(bool c) { auto r = HiddenReference(c ? alpha : beta); }
void receiver(bool c) { HiddenReference(c ? alpha : beta).run(); }
}
void boundary_alias_control() {
    BoundaryCallback a = alpha; BoundaryCallback f = HiddenReference(a); f();
}
using BoundaryReference = BoundaryCallback&;
void boundary_functional_store(bool c) {
    BoundaryCallback a = beta, b = beta;
    BoundaryReference(c ? a : b) = alpha; a(); b();
}
void boundary_named_store(bool c) {
    BoundaryCallback a = beta, b = beta;
    static_cast<BoundaryCallback&>(c ? a : b) = alpha; a(); b();
}
void boundary_assignment_arm_store(bool c, bool d) {
    BoundaryCallback a = beta, b = beta;
    BoundaryCallback f = c ? (BoundaryReference(d ? a : b) = alpha) : beta;
    a(); b(); f();
}
using BoundaryPointerReference = BoundaryCallback*&;
void boundary_cast_advance(bool c, BoundaryCallback *a, BoundaryCallback *b) {
    BoundaryPointerReference(c ? a : b) += 0;
    static_cast<BoundaryCallback*&>(c ? a : b) += 0;
}
void boundary_cstyle_consume(BoundaryCallback *p) { (*p)(); }
void boundary_cstyle_address(bool c) {
    BoundaryCallback a = alpha, b = beta;
    boundary_cstyle_consume(&((BoundaryCallback&)(c ? a : b)));
}
BoundaryCallback boundary_return_cell = alpha;
BoundaryCallback *boundary_return_pointer = &boundary_return_cell;
BoundaryCallback *&boundary_pointer_get() { return boundary_return_pointer; }
void boundary_pointer_return_consume(BoundaryCallback **v) { BoundaryCallback *q = *v; (*q)(); }
void boundary_pointer_return() { boundary_pointer_return_consume(&boundary_pointer_get()); }
BoundaryCallback **boundary_return_pp = &boundary_return_pointer;
BoundaryCallback *&boundary_deref_pointer_get() { return *boundary_return_pp; }
void boundary_deref_pointer_value() { BoundaryCallback *p = boundary_deref_pointer_get(); (*p)(); }
void boundary_deref_pointer_address_consume(BoundaryCallback **v) { BoundaryCallback *q = *v; (*q)(); }
void boundary_deref_pointer_address() { boundary_deref_pointer_address_consume(&boundary_deref_pointer_get()); }
auto boundary_trailing_get() -> BoundaryReference { return boundary_return_cell; }
void boundary_trailing_return() { BoundaryCallback *q = &boundary_trailing_get(); (*q)(); }
BoundaryReference boundary_alias_get() { return boundary_return_cell; }
void boundary_alias_return_control() { BoundaryCallback *q = &boundary_alias_get(); (*q)(); }
auto boundary_trailing_pointer_get() -> BoundaryCallback*& { return boundary_return_pointer; }
void boundary_trailing_pointer_consume(BoundaryCallback **v) { BoundaryCallback *q = *v; (*q)(); }
void boundary_trailing_pointer_return() { boundary_trailing_pointer_consume(&boundary_trailing_pointer_get()); }
void boundary_cast_array_init(bool c) {
    BoundaryCallback table[1] = {static_cast<BoundaryCallback>(c ? alpha : beta)}; table[0]();
}
void boundary_cstyle_array_init(bool c) {
    BoundaryCallback table[1] = {(const BoundaryCallback)(c ? alpha : beta)}; table[0]();
}
struct BoundaryOps { BoundaryCallback cb; BoundaryCallback other; };
struct BoundarySingleOps { BoundaryCallback cb; };
void boundary_single_struct_array(bool c) {
    BoundarySingleOps table[1] = {{c ? alpha : beta}}; table[0].cb();
}
void boundary_struct_array_init(bool c) {
    BoundaryOps table[1] = {{c ? alpha : beta, unrelated}}; table[0].cb();
}
void boundary_struct_array_other(bool c) {
    BoundaryOps table[1] = {{c ? alpha : beta, unrelated}}; table[0].other();
}
void boundary_struct_array_control(bool c) {
    BoundaryOps table = {c ? alpha : beta, unrelated}; table.cb();
}
void boundary_struct_array_elided(bool c) {
    BoundaryOps table[1] = {c ? alpha : beta, unrelated}; table[0].cb();
}
void boundary_struct_array_nested(bool c) {
    BoundaryOps table[1][1] = {{{c ? alpha : beta, unrelated}}}; table[0][0].cb();
}
extern bool boundary_condition;
struct BoundaryGlobalOps { BoundaryCallback cb; };
BoundaryGlobalOps boundary_global_table[1] = {{boundary_condition ? alpha : beta}};
void boundary_global_struct_array() { boundary_global_table[0].cb(); }
BoundaryCallback boundary_global_arm_cell = unrelated;
BoundaryCallback boundary_global_arm_value = boundary_condition ? ((boundary_global_arm_cell = alpha), beta) : beta;
void boundary_global_arm_effect() { boundary_global_arm_cell(); }
void boundary_global_arm_result() { boundary_global_arm_value(); }
BoundaryCallback boundary_global_comma_cell = unrelated;
BoundaryCallback boundary_global_comma_value = ((boundary_global_comma_cell = alpha), beta);
void boundary_global_comma_effect() { boundary_global_comma_cell(); }
void boundary_global_comma_result() { boundary_global_comma_value(); }
BoundaryCallback boundary_global_condition_cell = unrelated;
BoundaryCallback boundary_global_condition_value = ((boundary_global_condition_cell = alpha), boundary_condition) ? beta : beta;
void boundary_global_condition_effect() { boundary_global_condition_cell(); }
void boundary_global_condition_result() { boundary_global_condition_value(); }
BoundaryCallback boundary_global_shorthand_cell = unrelated;
BoundaryCallback boundary_global_shorthand_value = (boundary_global_shorthand_cell = alpha) ?: beta;
void boundary_global_shorthand_effect() { boundary_global_shorthand_cell(); }
void boundary_global_shorthand_result() { boundary_global_shorthand_value(); }
BoundaryCallback boundary_global_unevaluated_cell = unrelated;
BoundaryCallback boundary_global_unevaluated_value =
    (sizeof(boundary_global_unevaluated_cell = alpha), beta);
void boundary_global_unevaluated_effect() { boundary_global_unevaluated_cell(); }
BoundaryCallback boundary_global_noexcept_cell = unrelated;
BoundaryCallback boundary_global_noexcept_value =
    (noexcept(boundary_global_noexcept_cell = alpha), beta);
void boundary_global_noexcept_effect() { boundary_global_noexcept_cell(); }
