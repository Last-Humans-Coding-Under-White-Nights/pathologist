using ThirteenF=void(*)();
using ThirteenG=ThirteenF&(*)();
using ThirteenP=ThirteenG*;
using ThirteenR=ThirteenF&;
void alpha(); void beta(); void unrelated(); void thirteen_touch() {}
ThirteenF thirteen_a=alpha;
ThirteenF &thirteen_get() { return thirteen_a; }
ThirteenF &thirteen_mixed(int) { return thirteen_a; }
ThirteenF thirteen_mixed(double) { return beta; }
ThirteenF thirteen_mixed_wrap() { return thirteen_mixed(1.0+0.0); }
void thirteen_uncertain_returns() { ThirteenF f=thirteen_mixed_wrap(); f(); }

void thirteen_nested_store(bool c) {
    ThirteenF a{},b{}; ThirteenF *p=&a,*q=&b; ThirteenF **x=&p,**y=&q;
    (c?x:y)[0][0]=alpha; a(); b();
}
void thirteen_nested_callee(bool c) {
    ThirteenF a=alpha,b=beta; ThirteenF *p=&a,*q=&b; ThirteenF **x=&p,**y=&q;
    (**(c?x:y))();
}
ThirteenF thirteen_table[2]={alpha,beta};
void thirteen_offset_table(int i) { (thirteen_table+i)[0](); }
void thirteen_offset_read(int i) { ThirteenF f=(thirteen_table+i)[0]; f(); }
ThirteenF *thirteen_offset_return(bool c) { return (c?thirteen_table:thirteen_table)+0; }
void thirteen_returned_offset(bool c) { ThirteenF *p=thirteen_offset_return(c); (*p)(); }
decltype(auto) thirteen_callable_offset_wrap(ThirteenG *g) { return (*(g+0))(); }
void thirteen_callable_offset() { ThirteenG g=thirteen_get; ThirteenF *p=&thirteen_callable_offset_wrap(&g); (*p)(); }
decltype(auto) thirteen_cast_table_wrap(ThirteenG *g) { return static_cast<ThirteenP>(g)[0](); }
void thirteen_cast_table() { ThirteenG g=thirteen_get; ThirteenF *p=&thirteen_cast_table_wrap(&g); (*p)(); }
decltype(auto) thirteen_deferred_wrap() { ThirteenG g; g=thirteen_get; return g(); }
void thirteen_deferred_callable() { ThirteenF *p=&thirteen_deferred_wrap(); (*p)(); }
enum ThirteenIndex { THIRTEEN_ZERO };
void thirteen_enumerator() { ThirteenF *p=thirteen_table+THIRTEEN_ZERO; (*p)(); }

void thirteen_use(ThirteenF *p) { (*p)(); }
void thirteen_comma_address(bool c) {
    ThirteenF a=alpha,b=beta; thirteen_use(&(c?(ThirteenR)(thirteen_touch(),a):b));
}
ThirteenR thirteen_comma_get() { return (ThirteenR)(thirteen_touch(),thirteen_a); }
void thirteen_comma_return() { ThirteenF *p=&thirteen_comma_get(); (*p)(); }
ThirteenF thirteen_global_cell=beta;
struct ThirteenDefaults { ThirteenF f=((thirteen_global_cell=alpha),beta); };
ThirteenDefaults thirteen_global_array[2]={{beta}};
void thirteen_omitted_array_default() { thirteen_global_cell(); }
ThirteenF thirteen_override_cell=beta;
struct ThirteenOverride { ThirteenF f=((thirteen_override_cell=unrelated),beta); };
ThirteenOverride thirteen_override_array[1]={{alpha}};
void thirteen_full_array_override() { thirteen_override_cell(); }

namespace ThirteenDefaultsScope {
ThirteenF a=alpha,b=beta;
ThirteenF &get(ThirteenF &v=a);
ThirteenF &get(ThirteenF &v) { return v; }
ThirteenF &explicit_get(ThirteenF &v=a) { return v; }
struct Member { ThirteenF &get(ThirteenF &v=b) { return v; } };
Member member;
}
ThirteenF *thirteen_default_free=&ThirteenDefaultsScope::get();
ThirteenF *thirteen_default_member=&ThirteenDefaultsScope::member.get();
ThirteenF *thirteen_explicit=&ThirteenDefaultsScope::explicit_get(ThirteenDefaultsScope::b);
void thirteen_free_default() { (*thirteen_default_free)(); }
void thirteen_member_default() { (*thirteen_default_member)(); }
void thirteen_explicit_default_override() { (*thirteen_explicit)(); }
namespace ThirteenDefaultEffects {
ThirteenF skipped=beta, evaluated=beta, a=alpha, b=beta;
ThirteenF &skip(ThirteenF &v=((skipped=unrelated),a)) { return v; }
ThirteenF &evaluate(ThirteenF &v=((evaluated=alpha),b)) { return v; }
}
ThirteenF *thirteen_skipped_default=&ThirteenDefaultEffects::skip(ThirteenDefaultEffects::b);
ThirteenF *thirteen_evaluated_default=&ThirteenDefaultEffects::evaluate();
void thirteen_skipped_default_effect() { ThirteenDefaultEffects::skipped(); }
void thirteen_evaluated_default_effect() { ThirteenDefaultEffects::evaluated(); }
