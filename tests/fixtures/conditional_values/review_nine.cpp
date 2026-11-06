using NineCallback = void(*)();
using NineGetter = NineCallback&(*)();
void alpha(); void beta(); void unrelated();
NineCallback nine_a=alpha, nine_b=beta;
NineCallback &nine_ga() { return nine_a; }
NineCallback &nine_gb() { return nine_b; }
decltype(auto) nine_wrap(bool c, NineGetter g, NineGetter h) { return (c?g:h)(); }
void nine_selected_getters(bool c) { NineCallback *p=&nine_wrap(c,nine_ga,nine_gb); (*p)(); }
struct NineGetterField { NineGetter g; };
decltype(auto) nine_field_wrap(NineGetterField s) { return s.g(); }
void nine_field_getter() { NineGetterField s{nine_ga}; NineCallback *p=&nine_field_wrap(s); (*p)(); }
struct NineMemberGetter { NineCallback &get(NineCallback &v) { return v; } };
NineMemberGetter nine_member;
NineCallback *nine_global_argument=&nine_member.get(nine_a);
void nine_member_argument() { (*nine_global_argument)(); }
NineCallback &nine_free_get();
NineCallback &nine_free_get(int) { return nine_b; }
NineCallback *nine_free_selected=&nine_free_get();
void nine_free_overload() { (*nine_free_selected)(); }
NineCallback nine_offsets[1]={alpha};
NineCallback &nine_offset_get(int i) { return *(nine_offsets+i+0); }
void nine_nested_offset() { NineCallback *p=&nine_offset_get(0); (*p)(); }
struct NineCallable { void operator()() {} };
void nine_deref_callable(bool c) { NineCallable a,b; NineCallable *p=&a,*q=&b; (*(c?p:q))(); }
struct NineObject { NineCallback f; NineCallback other; };
struct NinePointer { NineObject *p; };
struct NinePointerArray { NineObject *p[1]; };
void nine_pointer_index() { NineObject s{alpha,unrelated}; NinePointer w{&s}; w.p[0].f(); }
void nine_pointer_array_index() { NineObject s{alpha,unrelated}; NinePointerArray w{{&s}}; w.p[0]->f(); }
struct NineDefaults { NineCallback f; NineCallback g=beta; };
void nine_designated_defaults(bool c) { NineDefaults a[1]={{.f=c?alpha:beta}}; a[0].g(); }
struct NineDefaultsOverride { NineCallback f; NineCallback g=unrelated; };
void nine_designated_override(bool c) { NineDefaultsOverride a[1]={{.f=c?alpha:beta,.g=alpha}}; a[0].g(); }
struct NineNestedDefaults { NineDefaults s; };
void nine_nested_defaults(bool c) { NineNestedDefaults a[1]={{.s={.f=c?alpha:beta}}}; a[0].s.g(); }
NineCallback nine_partial_cell=beta, nine_designated_cell=beta, nine_override_cell=beta;
struct NinePartial { int i; NineCallback f=((nine_partial_cell=alpha),beta); };
struct NinePartialDesignated { int i; NineCallback f=((nine_designated_cell=alpha),beta); };
struct NinePartialOverride { int i; NineCallback f=((nine_override_cell=unrelated),beta); };
NinePartial nine_partial{1};
NinePartialDesignated nine_partial_designated{.i=1};
NinePartialOverride nine_partial_override{.i=1,.f=alpha};
void nine_partial_global() { nine_partial_cell(); }
void nine_partial_designated_global() { nine_designated_cell(); }
void nine_partial_override_global() { nine_override_cell(); }

NineCallback nine_stored_cell=alpha;
struct NineStoreGetter { NineCallback &get(NineCallback &v) { return v; } };
NineStoreGetter nine_store_getter;
NineCallback *nine_store_address=&nine_store_getter.get(nine_stored_cell);
void nine_store_through_argument() { *nine_store_address=beta; nine_stored_cell(); }
