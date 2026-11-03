using OffsetReviewF = void (*)();
void alpha();
void beta();
void unrelated();
OffsetReviewF offset_review_table[2] = {alpha, beta};
struct OffsetReviewHost {
    enum Kind { X, Y };
    void member(Kind k) { OffsetReviewF f = *(offset_review_table + k); f(); }
    void constant() { OffsetReviewF f = *(offset_review_table + Y); f(); }
};
void offset_review_class_parameter(OffsetReviewHost::Kind k) {
    OffsetReviewF f = *(offset_review_table + k); f();
}
void offset_review_class_constant() {
    OffsetReviewF f = *(offset_review_table + OffsetReviewHost::Y); f();
}
namespace OffsetReviewEnums { enum Mode { P, Q }; }
using namespace OffsetReviewEnums;
void offset_review_imported(Mode m) { OffsetReviewF f = *(offset_review_table + m); f(); }
void offset_review_imported_constant() { OffsetReviewF f = *(offset_review_table + Q); f(); }
void offset_review_arithmetic(int i, long l) {
    OffsetReviewF a = *(offset_review_table + i * 1);
    OffsetReviewF b = *(offset_review_table + i % 2);
    OffsetReviewF c = *(offset_review_table + (i & 1));
    OffsetReviewF d = *(offset_review_table + (int)l);
    OffsetReviewF e = *(offset_review_table + sizeof(int) / sizeof(int));
    OffsetReviewF f = *(offset_review_table + ((i << 1) | (i ^ 1)));
    OffsetReviewF g = *(offset_review_table + static_cast<int>(l));
    OffsetReviewF h = *(offset_review_table + ~i);
    a(); b(); c(); d(); e(); f(); g(); h();
}

OffsetReviewF normal_default_cell = alpha;
OffsetReviewF &normal_default_get(OffsetReviewF &x = normal_default_cell) { return x; }
void normal_default_callp(OffsetReviewF *p) { (*p)(); }
void normal_default_reference_caller() { normal_default_callp(&normal_default_get()); }
void normal_default_take(OffsetReviewF f = alpha) { f(); }
void normal_default_value_caller() { normal_default_take(); }
struct NormalDefaultMethod { void run(OffsetReviewF f = alpha) { f(); } };
void normal_default_method_caller() { NormalDefaultMethod m; m.run(); }
struct NormalDefaultCtor {
    OffsetReviewF f;
    NormalDefaultCtor(OffsetReviewF value = alpha) : f(value) {}
    void run() { f(); }
};
void normal_default_ctor_caller() { NormalDefaultCtor s; s.run(); }
struct NormalDefaultHeapCtor { NormalDefaultHeapCtor(OffsetReviewF f = alpha) { f(); } };
void normal_default_heap_caller() { new NormalDefaultHeapCtor; }
OffsetReviewF normal_default_skipped = beta;
void normal_default_skip(int = ((normal_default_skipped = unrelated), 0), OffsetReviewF f = alpha) { f(); }
void normal_default_skip_caller() { normal_default_skip(1); }
void normal_default_skipped_check() { normal_default_skipped(); }
void normal_default_override(OffsetReviewF f = alpha) { f(); }
void normal_default_override_caller() { normal_default_override(beta); }
OffsetReviewF normal_default_make() { return alpha; }
void normal_default_nested_take(OffsetReviewF f = normal_default_make()) { f(); }
void normal_default_nested_caller() { normal_default_nested_take(); normal_default_nested_take(); }
namespace NormalDefaultScope {
OffsetReviewF value = alpha;
void take(OffsetReviewF f = value) { f(); }
}
void normal_default_scope_caller() { OffsetReviewF value = beta; NormalDefaultScope::take(); }
struct NormalDefaultReferenceMember {
    OffsetReviewF &get(OffsetReviewF &x = normal_default_cell) { return x; }
};
void normal_default_member_ref_caller() {
    NormalDefaultReferenceMember m;
    OffsetReviewF *p = &m.get();
    (*p)();
}
struct NormalDefaultVirtualBase {
    virtual void run(OffsetReviewF f = alpha) { f(); }
};
struct NormalDefaultVirtualDerived : NormalDefaultVirtualBase {
    void run(OffsetReviewF f = beta) override { f(); }
};
void normal_default_virtual_caller(NormalDefaultVirtualBase *p) { p->run(); }
struct NormalDefaultAbstract {
    virtual void run(OffsetReviewF f = alpha) = 0;
};
struct NormalDefaultConcrete : NormalDefaultAbstract {
    void run(OffsetReviewF f) override { f(); }
};
void normal_default_abstract_caller(NormalDefaultAbstract *p) { p->run(); }
OffsetReviewF &normal_default_reference_source() { return normal_default_cell; }
void normal_default_reference_take(OffsetReviewF &f = normal_default_reference_source()) { f(); }
void normal_default_reference_source_caller() { normal_default_reference_take(); }
