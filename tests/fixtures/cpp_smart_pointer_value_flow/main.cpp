// Value flow through smart pointers (#141). Standard wrappers are spelled
// without their headers, as in the other smart-pointer fixtures.
struct Payload { void (*cb)(); int *value; };
void PayloadTarget() {}
void seed(Payload *p) { p->cb = PayloadTarget; }

// `sp->field` reads the pointee through the wrapper value it was copied from.
void read(std::shared_ptr<Payload> input) {
    auto sp = input;
    auto value = sp->value;
    sp->cb();
}

// A wrapper-valued member is loaded before it is unwrapped.
struct Holder { std::shared_ptr<Payload> item; };
void nested(Holder *h) { auto nested_value = h->item->value; }
void nested_dot(Holder h) { auto dot_value = h.item->value; }
void twice(std::shared_ptr<Holder> h) { auto twice_value = h->item->value; }
// `(*x).field` on a wrapper-valued member dereferences the member.
void deref_dot(Holder h) { auto deref_dot_value = (*h.item).value; }
void deref_twice(std::shared_ptr<Holder> h) { auto deref_twice_value = (*h->item).value; }

// A method name is not a field: decomposition must leave nothing behind.
struct Widget { void Run(); };
void method_probe(std::shared_ptr<Widget> w) { w->Run(); }

// A recognized weak-pointer promotion may alias its receiver's value.
struct Message { std::weak_ptr<Payload> weak; };
struct Sink { std::shared_ptr<Payload> strong; };
void promote_local(std::weak_ptr<Payload> wp) {
    auto promoted = wp.lock();
    auto promoted_value = promoted->value;
}
void promote_assign(std::weak_ptr<Payload> wp) {
    std::shared_ptr<Payload> assigned;
    assigned = wp.lock();
    auto assigned_value = assigned->value;
}
void promote_field(Message *msg) {
    auto field_promoted = msg->weak.lock();
    auto field_value = field_promoted->value;
}
void promote_paren(Message *msg) { auto paren_promoted = (msg->weak).lock(); }
void promote_into_field(std::weak_ptr<Payload> wp, Sink *s) { s->strong = wp.lock(); }
void promote_into_field_paren(std::weak_ptr<Payload> wp, Sink *s) { s->strong = (wp.lock()); }
void promote_into_deref(std::weak_ptr<Payload> wp, std::shared_ptr<Payload> *out) {
    *out = wp.lock();
}
void promote_ohos(OHOS::wptr<Payload> wp) {
    auto strong = wp.promote();
    strong->cb();
}
void promote_nested_ns(OHOS::Camera::wptr<Payload> wp) { auto nested_strong = wp.promote(); }

// Not promotions: another class's `lock`, a defined wrapper's own
// `promote`, and a call with an argument.
struct Mutex { Payload *lock(); };
void mutex_lock(Mutex m) { auto locked = m.lock(); }
namespace custom {
Payload CustomTarget;
template<class T> class wptr {
public:
    T *promote() { return &CustomTarget; }
};
}
void promote_custom(custom::wptr<Payload> wp) { auto custom_strong = wp.promote(); }
void promote_with_arg(std::weak_ptr<Payload> wp) { auto with_arg = wp.lock(0); }

// Receivers that are themselves values to compute: a call's result and a
// dereferenced pointer member. A parenthesized promotion is still one.
std::weak_ptr<Payload> g_weak;
std::weak_ptr<Payload> get_weak() { return g_weak; }
void promote_call_result() { auto call_promoted = get_weak().lock(); }
struct WeakRef { std::weak_ptr<Payload> *weak; };
void promote_deref_field(WeakRef *h) { auto deref_promoted = (*h->weak).lock(); }
void promote_into_deref_paren(std::weak_ptr<Payload> wp, std::shared_ptr<Payload> *out) {
    *out = (wp.lock());
}

// A reference receiver is read through.
void promote_ref(const OHOS::wptr<Payload> &weak) { auto ref_strong = weak.promote(); }
// The held class is unknown, so the result stays untyped; its value still
// aliases the receiver.
void promote_unnamed(std::weak_ptr<Missing> wp) { auto unnamed = wp.lock(); }

// A wrapper variable holds its pointee's address: what reaches its storage
// reaches its value and back, and an argument reaches the parameter.
Payload g_filled;
Payload g_passed;
Payload g_copied;
void Fill(OHOS::sptr<Payload> *out) { *out = &g_filled; }
void ReadFilled() { OHOS::sptr<Payload> filled; Fill(&filled); filled->cb(); }
void UseArg(OHOS::sptr<Payload> arg) { arg->cb(); }
void PassArg() { OHOS::sptr<Payload> passed = &g_passed; UseArg(passed); }
// The argument's object arrives only after the call (flow-insensitively):
// a one-time snapshot at wiring would miss it.
Payload g_late;
void FillLate(OHOS::sptr<Payload> *out) { *out = &g_late; }
void UseLate(OHOS::sptr<Payload> arg) { arg->cb(); }
void PassLate() { OHOS::sptr<Payload> late; UseLate(late); FillLate(&late); }
void ReadThrough() {
    OHOS::sptr<Payload> copied = &g_copied;
    OHOS::sptr<Payload> *w = &copied;
    (*w)->cb();
}

// A field path may start at a call's result.
Payload g_returned;
OHOS::sptr<Payload> GetSp() { OHOS::sptr<Payload> r = &g_returned; return r; }
void ReadReturned() {
    GetSp()->cb();
    auto returned_value = GetSp()->value;
}
