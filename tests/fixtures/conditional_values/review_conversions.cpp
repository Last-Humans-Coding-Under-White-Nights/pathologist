using ConversionCallback = void (*)();
void alpha(); void beta(); void unrelated();
using ConversionReference = ConversionCallback&;
ConversionReference conversion_alias_get();
decltype(auto) conversion_alias_wrap() { return conversion_alias_get(); }
ConversionCallback conversion_alias_cell = alpha;
ConversionReference conversion_alias_get() { return conversion_alias_cell; }
void conversion_prototype_alias() { ConversionCallback *p = &conversion_alias_wrap(); (*p)(); }
ConversionCallback &conversion_explicit_get();
decltype(auto) conversion_explicit_wrap() { return conversion_explicit_get(); }
ConversionCallback &conversion_explicit_get() { return conversion_alias_cell; }
void conversion_prototype_control() { ConversionCallback *p = &conversion_explicit_wrap(); (*p)(); }
void conversion_pointer_cast_read(bool c) {
    ConversionCallback a[1] = {alpha}, b[1] = {beta};
    ConversionCallback f = static_cast<ConversionCallback*>(c ? a : b)[0]; f();
}
void conversion_pointer_cast_callee(bool c) {
    ConversionCallback a[1] = {alpha}, b[1] = {beta};
    static_cast<ConversionCallback*>(c ? a : b)[0]();
}
void conversion_pointer_cast_consume(ConversionCallback *p) { (*p)(); }
void conversion_pointer_cast_address(bool c) {
    ConversionCallback a[1] = {alpha}, b[1] = {beta};
    conversion_pointer_cast_consume(&static_cast<ConversionCallback*>(c ? a : b)[0]);
}
void conversion_pointer_cast_store(bool c) {
    ConversionCallback a[1] = {nullptr}, b[1] = {nullptr};
    static_cast<ConversionCallback*>(c ? a : b)[0] = alpha; a[0](); b[0]();
}
struct ConversionIndexer {
    explicit ConversionIndexer(ConversionCallback *) {}
    ConversionCallback operator[](int) { return unrelated; }
};
void conversion_object_cast_table(bool c) {
    ConversionCallback a[1] = {alpha}, b[1] = {beta};
    ConversionCallback f = static_cast<ConversionIndexer>(c ? a : b)[0]; f();
}
struct ConversionArrayOps { ConversionCallback f; };
void conversion_aggregate_element(bool c) {
    ConversionArrayOps a[1] = {{alpha}}, b[1] = {{beta}}; (c ? a : b)[0].f();
}
void conversion_aggregate_control(bool c) {
    ConversionArrayOps a[1] = {{alpha}}, b[1] = {{beta}}; (&(c ? a : b)[0])->f();
}
struct ConversionPreciseOps { ConversionCallback f; ConversionCallback other; };
void conversion_aggregate_precise(bool c) {
    ConversionPreciseOps a[1] = {{c ? alpha : alpha, unrelated}}, b[1] = {{c ? beta : beta, unrelated}};
    (c ? a : b)[0].f();
}
struct ConversionDesignatedOps { ConversionCallback f; ConversionCallback other; };
void conversion_designated_array(bool c) {
    ConversionDesignatedOps a[1] = {{.f = c ? alpha : beta, .other = unrelated}}; a[0].f();
}
void conversion_designated_other(bool c) {
    ConversionDesignatedOps a[1] = {{.f = c ? alpha : beta, .other = unrelated}}; a[0].other();
}
struct ConversionRun { void run() {} };
void conversion_null_array(bool c) { ConversionRun a[1]; (c ? a : nullptr)->run(); }
void conversion_null_array_auto(bool c) { ConversionRun a[1]; auto p = c ? a : nullptr; p->run(); }
void conversion_null_array_reverse(bool c) { ConversionRun a[1]; (c ? nullptr : a)->run(); }
void conversion_null_array_control(bool c) { ConversionRun a[1]; ConversionRun *p = c ? a : nullptr; p->run(); }
struct ConversionBase { void run() {} };
struct ConversionDerived : ConversionBase {};
void conversion_base_lvalue(bool c) { ConversionDerived d; ConversionBase b; (c ? d : b).run(); }
void conversion_base_lvalue_control(bool c) { ConversionDerived d; ConversionBase b; ConversionBase &r = c ? d : b; r.run(); }
ConversionDerived conversion_derived;
ConversionBase conversion_base;
decltype(auto) conversion_base_get(bool c) { return c ? conversion_derived : conversion_base; }
void conversion_rvalue_function_reference(bool c) {
    ConversionCallback f = &(c ? static_cast<void(&&)()>(alpha) : beta); f();
}
using ConversionFunction = void();
void conversion_rvalue_function_alias(bool c) {
    ConversionCallback f = &(c ? static_cast<ConversionFunction&&>(alpha) : beta); f();
}
ConversionRun conversion_xvalue_object;
decltype(auto) conversion_object_xvalue() { return static_cast<ConversionRun&&>(conversion_xvalue_object); }
extern bool conversion_condition;
ConversionCallback conversion_address_cell = unrelated, conversion_address_other = beta;
ConversionCallback *conversion_address_pointer = &((conversion_address_cell = alpha), conversion_address_other);
void conversion_address_effect() { conversion_address_cell(); }
void conversion_address_result() { (*conversion_address_pointer)(); }
ConversionCallback conversion_condition_cell = unrelated, conversion_condition_other = beta;
ConversionCallback *conversion_condition_pointer = &(((conversion_condition_cell = alpha), conversion_condition) ? conversion_address_other : conversion_condition_other);
void conversion_address_condition_effect() { conversion_condition_cell(); }
void conversion_address_condition_result() { (*conversion_condition_pointer)(); }
ConversionCallback conversion_decay_cell = unrelated, conversion_decay_table[1] = {beta};
ConversionCallback conversion_decay_value = ((conversion_decay_cell = alpha), conversion_decay_table)[0];
void conversion_decay_effect() { conversion_decay_cell(); }
void conversion_decay_result() { conversion_decay_value(); }
int conversion_index(ConversionCallback f) { return 0; }
ConversionCallback conversion_index_cell = unrelated, conversion_index_table[1] = {beta};
ConversionCallback conversion_index_value = (true ? conversion_index_table : conversion_index_table)[conversion_index(conversion_index_cell = alpha)];
void conversion_index_effect() { conversion_index_cell(); }
void conversion_index_result() { conversion_index_value(); }
ConversionCallback conversion_named_index_cell = unrelated;
ConversionCallback conversion_named_index_value = conversion_index_table[conversion_index(conversion_named_index_cell = alpha)];
void conversion_named_index_effect() { conversion_named_index_cell(); }
ConversionCallback conversion_lambda_cell = beta, conversion_lambda_body_cell = beta;
ConversionCallback conversion_lambda_value = ([x = (conversion_lambda_cell = alpha)]() { conversion_lambda_body_cell = alpha; }, beta);
void conversion_lambda_effect() { conversion_lambda_cell(); }
void conversion_lambda_body_control() { conversion_lambda_body_cell(); }
ConversionCallback conversion_nested_capture_cell = beta;
ConversionCallback conversion_nested_capture_value = ([x = [y = (conversion_nested_capture_cell = alpha)]() {}]() {}, beta);
void conversion_nested_capture_effect() { conversion_nested_capture_cell(); }
