using Callback = void (*)();
void alpha();
void beta();
void unrelated();
Callback touch();

void conditional_advance(bool c, int *p, int *q) { (c ? p : q) += 1; }
void comma_advance(int *p) { (touch(), p) += 1; }
void assignment_advance(int *p, int *q) { (p = q) += 1; }
void address_advance(bool c, int *p, int *q) { auto *x = &((c ? p : q) += 1); }
void conditional_subtract(bool c, int *p, int *q) { (c ? p : q) -= 1; }
void scalar_compound(bool c, int p, int q) { (c ? p : q) *= 2; }
void consume_advanced(Callback *p) { (*p)(); }
void advancing_value(bool c) {
    Callback a = alpha, b = beta;
    Callback *p = &a, *q = &b;
    consume_advanced(((c ? p : q) += 0));
}
void consume_advanced_address(Callback **p) { Callback *q = *p; (*q)(); }
void advancing_address(bool c) {
    Callback a = alpha, b = beta;
    Callback *p = &a, *q = &b;
    consume_advanced_address(&((c ? p : q) += 0));
}

void consume_selected_element(Callback *p) { (*p)(); }
void selected_element_address(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta}, noise[1] = {unrelated};
    consume_selected_element(&(c ? a : b)[0]);
}
void selected_element_read(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta}, noise[1] = {unrelated};
    Callback f = (c ? a : b)[0]; f();
}
void selected_element_callee(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta};
    ((c ? a : b)[0])();
}
void selected_element_store(bool c) {
    Callback a[1] = {nullptr}, b[1] = {nullptr};
    (c ? a : b)[0] = alpha;
    a[0](); b[0]();
}
void selected_pointer_element(bool c) {
    Callback a = alpha, b = beta;
    Callback *pa = &a, *pb = &b;
    Callback f = (c ? pa : pb)[0]; f();
}
void consume_comma_element(Callback *p) { (*p)(); }
void comma_element_address() {
    Callback a[1] = {alpha};
    consume_comma_element(&(touch(), a)[0]);
}
void selected_array_lengths(bool c) {
    Callback a[1] = {alpha}, b[2] = {beta, beta};
    ((c ? a : b)[0])();
}
Callback returned_elements_a[1] = {alpha}, returned_elements_b[1] = {beta};
Callback selected_element_return(bool c) { return (c ? returned_elements_a : returned_elements_b)[0]; }
void returned_selected_element(bool c) { Callback f = selected_element_return(c); f(); }
void consume_plain_element(Callback *p) { (*p)(); }
void plain_element_address() { Callback a[1] = {alpha}; consume_plain_element(&a[0]); }

struct DispatchBase { int n; virtual void run() {} };
struct DispatchDerived : DispatchBase { void run() override {} };
struct DispatchLeaf : DispatchDerived {};
struct DispatchOther { void run() {} };
void conditional_base_receiver(bool c) { DispatchBase a; DispatchDerived b; (c ? &b : &a)->run(); }
void conditional_base_reversed(bool c) { DispatchBase a; DispatchDerived b; (c ? &a : &b)->run(); }
void conditional_base_control(bool c) { DispatchBase a; DispatchDerived b; DispatchBase *p = c ? &b : &a; p->run(); }
void conditional_base_transitive(bool c) { DispatchBase a; DispatchLeaf b; (c ? &b : &a)->run(); }
void conditional_base_cast(bool c) { DispatchBase a; DispatchDerived b; (c ? static_cast<DispatchBase*>(&b) : &a)->run(); }
void conditional_base_c_cast(bool c) { DispatchBase a; DispatchDerived b; (c ? (DispatchBase*)&b : &a)->run(); }

Callback deduced_a = alpha, deduced_b = beta;
decltype(auto) deduced_reference_a() { return (deduced_a); }
auto deduced_reference_b() -> decltype(auto) { return (deduced_b); }
decltype(auto) deduced_reference_selector(bool c) { return c ? deduced_a : deduced_b; }
decltype(auto) deduced_reference_deref(Callback *p) { return *p; }
decltype(auto) deduced_value() { return deduced_a; }
decltype(auto) deduced_scalar() { return (1); }
void consume_deduced_reference(Callback *p) { (*p)(); }
void selected_deduced_references(bool c) {
    consume_deduced_reference(&(c ? deduced_reference_a() : deduced_reference_b()));
}
void consume_deduced_selector(Callback *p) { (*p)(); }
void deduced_selector_address(bool c) { consume_deduced_selector(&deduced_reference_selector(c)); }
void consume_deduced_deref(Callback *p) { (*p)(); }
void deduced_deref_address() { Callback a = alpha; consume_deduced_deref(&deduced_reference_deref(&a)); }
void ordinary_deduced_value() { Callback f = deduced_value(); f(); }
Callback deduced_empty = nullptr;
decltype(auto) deduced_empty_reference() { return (deduced_empty); }
void write_deduced_reference() { Callback *p = &deduced_empty_reference(); *p = alpha; deduced_empty(); }

int element_condition() { return 1; }
int element_index() { return 0; }
Callback *element_alpha() { return &deduced_a; }
Callback *element_beta() { return &deduced_b; }
void selected_element_call_arms() {
    ((element_condition() ? element_alpha() : element_beta())[element_index()])();
}
void selected_pointer_assignment(bool c) {
    Callback a = nullptr, b = nullptr;
    Callback *p = &a, *q = &b;
    (c ? p : (p = q))[0] = alpha;
    a(); b();
}
using Unary = void (*)(int);
void unary_target(int) {}
void binary_unrelated(int, int) {}
void array_address_signature_guard() {
    Unary a[1] = {unary_target};
    Unary *p = &a[0];
    *p = (Unary)binary_unrelated;
    a[0](1);
}

void consume_pointer_subscript_arms(Callback *p) { (*p)(); }
void pointer_subscript_arms(bool c) {
    Callback a = alpha, b = beta, other = unrelated;
    Callback *p = &a, *q = &b, *noise = &other;
    consume_pointer_subscript_arms(&(c ? p[0] : q[0]));
}
void consume_pointer_subscript_control(Callback *p) { (*p)(); }
void pointer_subscript_control(bool c) {
    Callback a = alpha, b = beta;
    Callback *p = &a, *q = &b;
    consume_pointer_subscript_control(&(c ? p : q)[0]);
}
void consume_plain_pointer_subscript(Callback *p) { (*p)(); }
void plain_pointer_subscript_address() {
    Callback a = alpha;
    Callback *p = &a;
    consume_plain_pointer_subscript(&p[0]);
}
void pointer_subscript_arm_store(bool c) {
    Callback a = nullptr, b = nullptr;
    Callback *p = &a, *q = &b;
    Callback *chosen = &(c ? p[0] : q[0]);
    *chosen = alpha;
    a(); b();
}
Callback *select_pointer_subscript(bool c, Callback *p, Callback *q) {
    return &(c ? p[0] : q[0]);
}
void consume_returned_pointer_subscript(Callback *p) { (*p)(); }
void returned_pointer_subscript_address(bool c) {
    Callback a = alpha, b = beta;
    consume_returned_pointer_subscript(select_pointer_subscript(c, &a, &b));
}
void consume_pointer_subscript_calls(Callback *p) { (*p)(); }
void pointer_subscript_call_arms() {
    consume_pointer_subscript_calls(&(element_condition()
        ? element_alpha()[element_index()] : element_beta()[element_index()]));
}

void consume_reference_cast(Callback *p) { (*p)(); }
void reference_cast_arm(bool c) {
    Callback a = alpha, b = beta, noise = unrelated;
    consume_reference_cast(&(c ? static_cast<Callback&>(a) : b));
}
void consume_c_reference_cast(Callback *p) { (*p)(); }
void c_reference_cast_arm(bool c) {
    Callback a = alpha, b = beta;
    consume_c_reference_cast(&(c ? (Callback&)a : b));
}
using CallbackReference = Callback&;
void consume_alias_reference_cast(Callback *p) { (*p)(); }
void alias_reference_cast_arm(bool c) {
    Callback a = alpha, b = beta;
    consume_alias_reference_cast(&(c ? static_cast<CallbackReference>(a) : b));
}
void consume_nested_reference_cast(Callback *p) { (*p)(); }
void nested_reference_cast_arm(bool c) {
    Callback a = alpha, b = beta;
    consume_nested_reference_cast(&(c ? const_cast<Callback&>(static_cast<const Callback&>(a)) : b));
}
void reference_cast_store(bool c) {
    Callback a = nullptr, b = nullptr;
    Callback *p = &(c ? static_cast<Callback&>(a) : b);
    *p = alpha;
    a(); b();
}
Callback cast_reference_cell = alpha;
Callback &reference_cast_getter() { return static_cast<Callback&>(cast_reference_cell); }
decltype(auto) deduced_reference_cast_getter() { return static_cast<Callback&>(cast_reference_cell); }
void consume_reference_cast_return(Callback *p) { (*p)(); }
void reference_cast_return_address() { consume_reference_cast_return(&reference_cast_getter()); }
void consume_deduced_reference_cast(Callback *p) { (*p)(); }
void deduced_reference_cast_address() { consume_deduced_reference_cast(&deduced_reference_cast_getter()); }
Callback cast_empty_cell = nullptr;
Callback &empty_reference_cast_getter() { return static_cast<Callback&>(cast_empty_cell); }
void reference_cast_return_store() { Callback *p = &empty_reference_cast_getter(); *p = beta; cast_empty_cell(); }
decltype(auto) reference_cast_value_control() { return static_cast<Callback>(cast_reference_cell); }

using CallbackFunction = void();
void explicit_function_reference_cast(bool c) {
    Callback f = &(c ? static_cast<void(&)()>(alpha) : beta); f();
}
void aliased_function_reference_cast(bool c) {
    Callback f = &(c ? static_cast<CallbackFunction&>(alpha) : beta); f();
}
void consume_functional_reference_cast(Callback *p) { (*p)(); }
void functional_reference_cast_arm(bool c) {
    Callback a = alpha, b = beta;
    consume_functional_reference_cast(&(c ? CallbackReference(a) : b));
}
namespace ReferenceCastNames { using R = Callback&; }
void consume_qualified_reference_cast(Callback *p) { (*p)(); }
void qualified_reference_cast_arm(bool c) {
    Callback a = alpha, b = beta;
    consume_qualified_reference_cast(&(c ? ReferenceCastNames::R(a) : b));
}
void functional_reference_cast_store(bool c) {
    Callback a = nullptr, b = nullptr;
    Callback *p = &(c ? CallbackReference(a) : b); *p = alpha; a(); b();
}
Callback &functional_reference_getter() { return CallbackReference(cast_reference_cell); }
void consume_functional_reference_return(Callback *p) { (*p)(); }
void functional_reference_return() { consume_functional_reference_return(&functional_reference_getter()); }
void consume_functional_cast_call(Callback *p) { (*p)(); }
void functional_cast_call() { consume_functional_cast_call(&CallbackReference(reference_cast_getter())); }
Callback &reference_identity(Callback &v) { return v; }
void functional_cast_shadowed_variable() {
    auto CallbackReference = reference_identity;
    Callback a = alpha;
    CallbackReference(a);
}
namespace ReferenceCastNames::Inner {
Callback &R(Callback &v) { return v; }
void real_call() { Callback a = alpha; R(a); }
}

void consume_functional_argument(Callback f) { f(); }
void functional_reference_argument(bool c) {
    Callback a = alpha, b = beta;
    consume_functional_argument(CallbackReference(c ? a : b));
}
void functional_reference_callee() { Callback a = alpha; (CallbackReference(a))(); }
using CompleteFunctionReference = void(&)();
typedef void (&TypedefFunctionReference)();
void complete_function_alias(bool c) {
    Callback f = &(c ? static_cast<CompleteFunctionReference>(alpha) : beta); f();
}
void typedef_function_alias(bool c) {
    Callback f = &(c ? static_cast<TypedefFunctionReference>(alpha) : beta); f();
}
void functional_complete_function_alias(bool c) {
    Callback f = &(c ? CompleteFunctionReference(alpha) : beta); f();
}
decltype(auto) function_selector(bool c) { return c ? alpha : beta; }
void returned_function_selector(bool c) { Callback f = &function_selector(c); f(); }
decltype(auto) nested_function_selector(bool c, bool d) { return c ? alpha : (d ? alpha : beta); }
void returned_nested_function_selector(bool c, bool d) { Callback f = &nested_function_selector(c,d); f(); }
Callback &get_reference_a();
Callback &get_reference_b();
extern bool file_selection_condition;
Callback *file_selected_reference = &(file_selection_condition ? get_reference_a() : get_reference_b());
Callback *file_simple_reference = &get_reference_a();
void file_reference_use() { (*file_selected_reference)(); }
void file_simple_reference_use() { (*file_simple_reference)(); }
Callback &remote_reference();
Callback *file_remote_reference = &remote_reference();
void file_remote_reference_use() { (*file_remote_reference)(); }
Callback file_empty_callback = nullptr;
Callback &file_empty_getter() { return file_empty_callback; }
Callback *file_reference_store_target = &file_empty_getter();
void file_reference_store() { *file_reference_store_target = beta; file_empty_callback(); }
Callback &parenthesized_functional_return(bool c) { return (CallbackReference(c ? deduced_a : deduced_b)); }
void read_parenthesized_functional_return(bool c) { Callback f = parenthesized_functional_return(c); f(); }
void parenthesized_functional_initializer() { Callback a = alpha; Callback f = (CallbackReference(a)); f(); }

void conditional_callback_array_init(bool c) {
    Callback table[1] = {c ? alpha : beta}; table[0]();
}
void conditional_callback_array_assignment_control(bool c) {
    Callback table[1]; table[0] = c ? alpha : beta; table[0]();
}
Callback array_init_alpha() { return alpha; }
Callback array_init_beta() { return beta; }
void conditional_callback_array_call_init(bool c) {
    Callback table[1] = {c ? array_init_alpha() : array_init_beta()}; table[0]();
}
void conditional_callback_array_nested_init(bool c, bool d) {
    Callback table[1][1] = {{c ? (d ? alpha : beta) : beta}}; table[0][0]();
}
void conditional_callback_array_null_init(bool c) {
    Callback table[1] = {c ? alpha : nullptr}; table[0]();
}
void consume_pointer_reference_cast(Callback **p) { Callback *q = *p; (*q)(); }
void pointer_reference_cast(bool c) {
    Callback a = alpha, b = beta;
    Callback *pa = &a, *pb = &b;
    consume_pointer_reference_cast(&(c ? static_cast<Callback*&>(pa) : pb));
}
void pointer_reference_cast_store(bool c) {
    Callback a = alpha, b = beta, replacement = nullptr;
    Callback *pa = &replacement, *pb = &replacement;
    Callback **p = &(c ? static_cast<Callback*&>(pa) : pb);
    *p = &a;
    Callback f = *pa; f();
    Callback g = *pb; g();
}
struct FunctionalReceiver { void run() {} };
using FunctionalReceiverReference = FunctionalReceiver&;
void functional_reference_receiver_arms(bool c) {
    FunctionalReceiver a, b;
    (c ? FunctionalReceiverReference(a) : FunctionalReceiverReference(b)).run();
}
void functional_reference_receiver_selector(bool c) {
    FunctionalReceiver a, b;
    FunctionalReceiverReference(c ? a : b).run();
}
void functional_reference_receiver_control(bool c) {
    FunctionalReceiver a, b; (c ? a : b).run();
}
using CallbackArrayReference = Callback(&)[1];
void functional_array_reference_read(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta};
    Callback f = CallbackArrayReference(c ? a : b)[0]; f();
}
void functional_array_reference_callee(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta};
    CallbackArrayReference(c ? a : b)[0]();
}
void functional_array_reference_store(bool c) {
    Callback a[1] = {nullptr}, b[1] = {nullptr};
    CallbackArrayReference(c ? a : b)[0] = alpha; a[0](); b[0]();
}
void consume_array_reference_address(Callback *p) { (*p)(); }
void functional_array_reference_address(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta};
    consume_array_reference_address(&CallbackArrayReference(c ? a : b)[0]);
}
void named_array_reference_read(bool c) {
    Callback a[1] = {alpha}, b[1] = {beta};
    Callback f = static_cast<Callback(&)[1]>(c ? a : b)[0]; f();
}
using CallbackPointerReference = Callback*&;
typedef Callback *&TypedefCallbackPointerReference;
void consume_pointer_reference_alias(Callback **p) { Callback *q = *p; (*q)(); }
void pointer_reference_alias(bool c) {
    Callback a = alpha, b = beta;
    Callback *pa = &a, *pb = &b;
    consume_pointer_reference_alias(&(c ? CallbackPointerReference(pa) : pb));
}
void consume_typedef_pointer_reference(Callback **p) { Callback *q = *p; (*q)(); }
void typedef_pointer_reference(bool c) {
    Callback a = alpha, b = beta;
    Callback *pa = &a, *pb = &b;
    consume_typedef_pointer_reference(&(c ? static_cast<TypedefCallbackPointerReference>(pa) : pb));
}
using PointerToReferenceGetter = Callback& (*)();
typedef Callback &(*TypedefPointerToReferenceGetter)();
void pointer_to_reference_return_control() {
    PointerToReferenceGetter p = static_cast<Callback& (*)()>(reference_cast_getter);
    Callback f = p(); f();
}
