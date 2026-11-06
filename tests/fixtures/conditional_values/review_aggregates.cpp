using AggregateCallback = void (*)();
void alpha(); void beta(); void unrelated();
void aggregate_anonymous(bool c) { struct { AggregateCallback f; } a[1]={{c?alpha:beta}}; a[0].f(); }
typedef struct { AggregateCallback f; } AggregateAnonymous;
void aggregate_anonymous_alias(bool c) { AggregateAnonymous a[1]={{c?alpha:beta}}; a[0].f(); }
union AggregateUnion { AggregateCallback f; int x; };
void aggregate_union(bool c) { AggregateUnion a[1]={{c?alpha:beta}}; a[0].f(); }
void aggregate_union_designated(bool c) { AggregateUnion a[1]={{.f=c?alpha:beta}}; a[0].f(); }
struct AggregateMember { AggregateCallback f; AggregateCallback other; };
struct AggregateVoidMember { AggregateCallback f; AggregateCallback other; };
void aggregate_void_pointer() { AggregateVoidMember a[1]={{alpha,unrelated}}; void *v=a; ((AggregateVoidMember*)v)[0].f(); }
void aggregate_const_void_pointer(int i) { AggregateVoidMember a[1]={{alpha,unrelated}}; void *v=a; ((const AggregateVoidMember*)v)[i].f(); }
void aggregate_multidimensional(bool c) { AggregateMember a[1][1]={{{alpha,unrelated}}}, b[1][1]={{{beta,unrelated}}}; (c?a:b)[0][0].f(); }
struct AggregateArrayField { AggregateCallback t[1]; AggregateCallback other; };
void aggregate_designated_array_field(bool c) { AggregateArrayField a[1]={{.t={c?alpha:beta},.other=unrelated}}; a[0].t[0](); }
struct AggregateNestedField { AggregateMember t[1]; };
void aggregate_nested_designated(bool c) { AggregateNestedField a[1]={{.t={{.f=c?alpha:beta,.other=unrelated}}}}; a[0].t[0].f(); }
struct AggregateConstructed { AggregateCallback f; AggregateConstructed(AggregateCallback x):f(x){} };
void aggregate_implicit_conversion(bool c) { AggregateConstructed a[1]={c?alpha:beta}; a[0].f(); }
void aggregate_braced_element(bool c) { AggregateConstructed a[1]={{c?alpha:beta}}; a[0].f(); }
using AggregatePointer = AggregateCallback*;
void aggregate_pointer_alias(bool c) { AggregateCallback a[1]={alpha}, b[1]={beta}; AggregateCallback f=AggregatePointer(c?a:b)[0]; f(); }
void aggregate_cast_alias(bool c) { AggregateCallback f=(AggregateCallback)(c?alpha:beta); f(); }
void aggregate_cast_callee(bool c) { ((AggregateCallback)(c?alpha:beta))(); }
void aggregate_cast_control() { AggregateCallback f=(AggregateCallback)(alpha); f(); }
void aggregate_multidimensional_use(AggregateCallback (*p)[1]) { p[0][0](); }
void aggregate_multidimensional_pass(bool c) { AggregateCallback a[1][1]={{alpha}}, b[1][1]={{beta}}; aggregate_multidimensional_use(c?a:b); }
AggregateCallback aggregate_ref_cell=alpha;
AggregateCallback &aggregate_ref_get() { return aggregate_ref_cell; }
using AggregateGetter = AggregateCallback&(*)();
decltype(auto) aggregate_ref_wrap(AggregateGetter g) { return g(); }
void aggregate_ref_indirect() { AggregateCallback *p=&aggregate_ref_wrap(aggregate_ref_get); (*p)(); }
AggregateCallback aggregate_ctor_effect=beta;
struct AggregateEffects { AggregateCallback f; AggregateEffects(AggregateCallback x):f(x){} };
AggregateEffects aggregate_effect_object(((aggregate_ctor_effect=alpha),beta));
void aggregate_ctor_effect_call() { aggregate_ctor_effect(); }
AggregateCallback aggregate_default_effect=beta;
struct AggregateDefault { AggregateCallback f=((aggregate_default_effect=alpha),beta); };
AggregateDefault aggregate_default_object;
void aggregate_default_effect_call() { aggregate_default_effect(); }
extern "C" void *dlsym(void*,const char*);
extern "C" void aggregate_string_alpha() {}
extern "C" void aggregate_string_beta() {}
void aggregate_string_null(bool c,void *h) { const char b[]="aggregate_string_beta"; const char *n=c?b:nullptr; AggregateCallback f=(AggregateCallback)dlsym(h,n); f(); }
void aggregate_string_choice(bool c,void *h) { const char b[]="aggregate_string_beta"; AggregateCallback f=(AggregateCallback)dlsym(h,c?b:"aggregate_string_alpha"); f(); }

using AggregateValueGetter = AggregateCallback*(*)();
AggregateCallback *aggregate_value_get() { return &aggregate_ref_cell; }
decltype(auto) aggregate_value_wrap(AggregateValueGetter g) { return g(); }
void aggregate_value_indirect() { AggregateCallback *p=aggregate_value_wrap(aggregate_value_get); (*p)(); }
namespace AggregateCastShadow {
struct AggregatePointer { AggregatePointer(AggregateCallback f) { f(); } };
void call(bool c) { auto value=AggregatePointer(c?alpha:beta); }
}

struct AggregateCopiedSlot { AggregateCallback f; };
void aggregate_constant_table_copy(int i) { AggregateCopiedSlot a[2]={{alpha},{beta}}, dst; dst.f=a[i].f; dst.f(); }
struct AggregateSeparatedSlot { AggregateCallback f; };
void aggregate_constant_tables_separate() { AggregateSeparatedSlot a[1]={{alpha}}, b[1]={{unrelated}}; a[0].f(); }
