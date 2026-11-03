typedef void (*Callback)(void);
void alpha(void) {}
void beta(void) {}
void unrelated(void) {}

void control(void) { Callback f = alpha; f(); }
void initializer(int c) { Callback f = c ? alpha : beta; f(); }
Callback choose(int c) { return c ? alpha : beta; }
void returned(int c) { Callback f = choose(c); f(); }
void direct_callee(int c) { (c ? alpha : beta)(); }
void invoke(Callback f) { f(); }
void argument(int c) { invoke(c ? alpha : beta); }
void assignment(int c) { Callback f; f = (c ? alpha : beta); f(); }
void nested(int c, int d) {
    Callback f = ((c ? (d ? alpha : beta) : (beta)));
    f();
}
Callback get_alpha(void) { return alpha; }
Callback get_beta(void) { return beta; }
int condition(void) { return 1; }
void call_arms(void) {
    Callback f = condition() ? get_alpha() : get_beta();
    f();
}
void call_callee(void) { (condition() ? get_alpha() : get_beta())(); }
Callback call_return(void) { return condition() ? get_alpha() : get_beta(); }
void returned_calls(void) { Callback f = call_return(); f(); }
void call_argument(void) { invoke(condition() ? get_alpha() : get_beta()); }
struct Ops { Callback callback; };
struct Other { int pad; Callback callback; };
void pointer_arms(int c) {
    struct Ops live = { alpha }, offline = { beta };
    struct Other noise = { 0, unrelated };
    struct Ops *selected = c ? &live : &offline;
    selected->callback();
}
void stores(int c) {
    Callback f, *out = &f;
    *out = c ? alpha : beta;
    f();
    struct Ops ops;
    ops.callback = c ? alpha : beta;
    ops.callback();
}
Callback cross_choose(int c);
void cross_returned(int c) { Callback f = cross_choose(c); f(); }
void unknown_arm(int c) {
    Callback unknown;
    Callback f = c ? alpha : unknown;
    f();
}
#define SELECT(c, a, b) ((c) ? (a) : (b))
void macro_conditional(int c) { Callback f = SELECT(c, alpha, beta); f(); }
void field_arms(int c) {
    struct Ops a = {alpha}, b = {beta};
    (c ? a.callback : b.callback)();
}
void address_arms(int c) { (c ? &alpha : &beta)(); }
void zero_arm(int c) { (c ? 0 : alpha)(); }
Callback conditional_factory(int c) { return (c ? get_alpha : get_beta)(); }
void returned_factory(int c) { Callback f = conditional_factory(c); f(); }
void constant_condition(void) { Callback f = 1 ? alpha : beta; f(); }

void shorthand_initializer(void) { Callback p = alpha; Callback f = p ?: beta; f(); }
void shorthand_callee(void) { Callback p = alpha; (p ?: beta)(); }
Callback shorthand_return(void) { Callback p = alpha; return p ?: beta; }
void shorthand_returned(void) { Callback f = shorthand_return(); f(); }
void shorthand_calls(void) { Callback f = get_alpha() ?: get_beta(); f(); }
void assignment_arm_callee(int c) {
    Callback p = unrelated;
    (c ? (p = alpha) : (p = beta))();
}
void assignment_arm_calls(int c) {
    Callback p;
    Callback f = c ? (p = get_alpha()) : (p = get_beta());
    f(); p();
}
void assignment_arm_store(int c) {
    Callback p, *out = &p;
    Callback f = c ? (*out = alpha) : (*out = beta);
    f(); p();
}
struct AssignmentHolder { Callback cb; };
void assignment_arm_field(int c) {
    struct AssignmentHolder h;
    Callback f = c ? (h.cb = alpha) : (h.cb = beta);
    f(); h.cb();
}
Callback assignment_arm_return(int c) {
    Callback p = unrelated;
    return c ? (p = alpha) : (p = beta);
}
void assignment_arm_returned(int c) { Callback f = assignment_arm_return(c); f(); }
Callback touch(void) { return unrelated; }
void comma_arms(int c) { (c ? (touch(), alpha) : (touch(), beta))(); }
void comma_call_arms(int c) {
    Callback f = c ? (touch(), get_alpha()) : (touch(), get_beta());
    f();
}
void comma_assignment_effects(int c) {
    Callback p;
    Callback f = c ? (p = beta, alpha) : (p = alpha, beta);
    f(); p();
}
