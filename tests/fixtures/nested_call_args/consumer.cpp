#include "common.h"

// 1. Direct call
void target_direct(void) {}
Callback make_direct(void) {
    return target_direct;
}
void consume_direct(Callback f) {
    f();
}
void entry_direct_nested(void) {
    consume_direct(make_direct());
}
void entry_direct_temp(void) {
    Callback f = make_direct();
    consume_direct(f);
}

// 2. Static call (scope-first resolution)
static void target_static(void) {}
static Callback make_static(void) {
    return target_static;
}
static void consume_static(Callback f) {
    f();
}
void entry_static_nested(void) {
    consume_static(make_static());
}
void entry_static_temp(void) {
    Callback f = make_static();
    consume_static(f);
}

// 3. Indirect inner call
typedef Callback (*Factory)(void);
Callback make_for_table(void) {
    return target_direct;
}
Factory g_table[1] = { make_for_table };
void consume_indirect(Callback f) {
    f();
}
void entry_indirect_nested(void) {
    consume_indirect(g_table[0]());
}
void entry_indirect_temp(void) {
    Callback f = g_table[0]();
    consume_indirect(f);
}

// 4. Member inner call
struct Obj {
    Callback GetHandler() {
        return target_direct;
    }
};
void consume_member(Callback f) {
    f();
}
void entry_member_nested(Obj *obj) {
    consume_member(obj->GetHandler());
}
void entry_member_temp(Obj *obj) {
    Callback f = obj->GetHandler();
    consume_member(f);
}

// 5. Deeper nesting
Callback pass_through(Callback f) {
    return f;
}
void consume_deep(Callback f) {
    f();
}
void entry_deep_nested(void) {
    consume_deep(pass_through(make_direct()));
}
void entry_deep_temp(void) {
    Callback f1 = make_direct();
    Callback f2 = pass_through(f1);
    consume_deep(f2);
}

// 6. std::move wrapping
void consume_move(Callback f) {
    f();
}
void entry_move_nested(void) {
    consume_move(std::move(make_direct()));
}
void entry_move_temp(void) {
    Callback f = std::move(make_direct());
    consume_move(f);
}

// 7. Cross-TU calls
void entry_cross_tu_nested(void) {
    consume_cross_tu(make_cross_tu());
}
void entry_cross_tu_temp(void) {
    Callback f = make_cross_tu();
    consume_cross_tu(f);
}
