using TenF = void(*)();
using TenG = TenF&(*)();
void alpha(); void beta(); void unrelated();
TenF ten_a=alpha, ten_b=beta;
TenF &ten_get() { return ten_a; }
TenF &ten_overload_get();
TenF &ten_overload_get(int) { return ten_b; }
TenF &ten_forward() { return ten_overload_get(); }
void ten_forward_caller() { TenF *p=&ten_forward(); (*p)(); }
void ten_forward_value() { TenF f=ten_forward(); f(); }

decltype(auto) ten_auto_cast_wrap() { auto g=static_cast<TenG>(ten_get); return g(); }
decltype(auto) ten_auto_address_wrap() { auto g=&ten_get; return g(); }
void ten_auto_cast() { TenF *p=&ten_auto_cast_wrap(); (*p)(); }
void ten_auto_address() { TenF *p=&ten_auto_address_wrap(); (*p)(); }
decltype(auto) ten_subscript_wrap(TenG *g) { return g[0](); }
void ten_subscript_getter() { TenG g[1]={ten_get}; TenF *p=&ten_subscript_wrap(g); (*p)(); }
using TenValueGetter=TenF(*)();
TenF ten_value_get() { return alpha; }
decltype(auto) ten_auto_value_wrap() { auto g=static_cast<TenValueGetter>(ten_value_get); return g(); }
void ten_auto_value() { TenF f=ten_auto_value_wrap(); f(); }

TenF ten_offsets_a[1]={alpha}, ten_offsets_b[1]={beta};
void ten_offset_arms(bool c) { TenF f=*(c ? ten_offsets_a+0 : ten_offsets_b+0); f(); }
void ten_offset_value(int i) { TenF *p=ten_offsets_a+i; (*p)(); }
void ten_offset_invoke(TenF *p) { (*p)(); }
void ten_offset_argument(int i) { ten_offset_invoke(ten_offsets_a+i); }
TenF ten_offset_callees[2]={alpha,beta};
void ten_offset_callee(int i) { (*(ten_offset_callees+i))(); }
enum TenIndex { TEN_FIRST, TEN_SECOND };
void ten_enum_offset(TenIndex i) { TenF f=*(ten_offsets_a+i); f(); }

TenF ten_union_cell=beta;
union TenInactive { int i=((ten_union_cell=unrelated),0); TenF f; };
void ten_inactive_union(bool c) { TenInactive a[1]={{.f=c ? beta : beta}}; ten_union_cell(); }
TenF ten_anon_cell=beta, ten_union_default_cell=beta, ten_second_union_cell=beta;
struct { TenF f=((ten_anon_cell=alpha),beta); } ten_anonymous;
union TenDefault { TenF f=((ten_union_default_cell=alpha),beta); int i; };
TenDefault ten_default_union;
union TenSecondDefault { int i; TenF f=((ten_second_union_cell=alpha),beta); };
TenSecondDefault ten_second_default_union;
void ten_anonymous_default() { ten_anon_cell(); }
void ten_union_default() { ten_union_default_cell(); }
void ten_second_union_default() { ten_second_union_cell(); }

TenF ten_comma_get() { return alpha; }
void ten_comma_cast() { TenF f=(TenF)(ten_comma_get(),beta); f(); }
void ten_comma_cast_callee() { ((TenF)(ten_comma_get(),beta))(); }
TenF ten_comma_effect_cell=beta;
TenF ten_comma_global=(TenF)((ten_comma_effect_cell=alpha),beta);
void ten_comma_global_effect() { ten_comma_effect_cell(); }
