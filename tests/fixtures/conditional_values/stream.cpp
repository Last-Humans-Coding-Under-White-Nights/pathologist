using Callback = void (*)();
void alpha();
void beta();
void unrelated();
struct Stream {
    Callback callback;
    Stream(Callback cb) : callback(cb) {}
};
Stream live(alpha), offline(beta);
Stream *live_stream() { return &live; }
Stream *offline_stream() { return &offline; }
Stream *select_stream(int c) {
    Stream *stream = c ? live_stream() : offline_stream();
    return stream;
}
struct Session { Callback callback; };
Session session = {alpha};
Session *front_stream() { return &session; }
Session *default_session(int empty) { return empty ? nullptr : front_stream(); }
void camera_stream(int c) { Stream *s = select_stream(c); s->callback(); }
void camera_session(int c) { Session *s = default_session(c); s->callback(); }
struct Repeat {
    Callback callback;
    Repeat(Callback cb) : callback(cb) {}
};
void camera_new(int c) {
    Repeat *s = c ? nullptr : new Repeat(beta);
    s->callback();
}
void auto_stream(int c) {
    auto s = c ? &live : &offline;
    s->callback();
}
void auto_null(int c) {
    auto s = c ? nullptr : &session;
    s->callback();
}
namespace std {
template<class T> struct shared_ptr {
    T *get() const;
    T *operator->() const;
    shared_ptr(T *);
};
}
struct Noise { int pad; Callback callback; };
void conditional_receiver(int c) {
    std::shared_ptr<Stream> a(new Stream(alpha)), b(new Stream(beta));
    std::shared_ptr<Noise> noise(new Noise{0, unrelated});
    (c ? a : b).get()->callback();
}
void null_callback(int c) {
    Callback f = c ? nullptr : alpha;
    f();
}
// Class operator+ typing is unavailable; the known arm still flows.
struct UnknownConditionalOffset {};
Stream *operator+(UnknownConditionalOffset, int);
void unknown_type_arm(int c) {
    Stream *s = c ? &live : (UnknownConditionalOffset{} + 0);
    s->callback();
}

// Address-of keeps the class layout; new and declared returns can omit it.
struct ConditionalShape { int n; void method() {} };
ConditionalShape shape_object;
ConditionalShape *shape_getter() { return &shape_object; }
void shape_new(int c) {
    auto p = c ? &shape_object : new ConditionalShape;
    p->method();
}
void shape_new_reversed(int c) {
    auto p = c ? new ConditionalShape : &shape_object;
    p->method();
}
void shape_get(int c) {
    auto p = c ? &shape_object : shape_getter();
    p->method();
}
void shape_get_reversed(int c) {
    auto p = c ? shape_getter() : &shape_object;
    p->method();
}
struct OtherShape { int n; };
void shape_mismatch(int c, void *unknown, OtherShape *other) {
    auto p = c ? &shape_object : unknown;
    auto q = c ? static_cast<void *>(&shape_object) : other;
}

void invoke_ptr(Callback *p) { (*p)(); }
void invoke_ptr_initializer(Callback *p) { (*p)(); }
void invoke_ptr_returned(Callback *p) { (*p)(); }
void invoke_ptr_fields(Callback *p) { (*p)(); }
void invoke_ptr_derefs(Callback *p) { (*p)(); }
void invoke_ptr_nested(Callback *p) { (*p)(); }
void address_conditional_argument(int c) {
    Callback a = alpha, b = beta, noise = unrelated;
    invoke_ptr(&(c ? a : b));
}
void address_conditional_initializer(int c) {
    Callback a = alpha, b = beta;
    Callback *selected = &(c ? a : b);
    invoke_ptr_initializer(selected);
}
Callback address_alpha = alpha, address_beta = beta;
Callback *address_conditional_return(int c) { return &(c ? address_alpha : address_beta); }
void address_conditional_returned(int c) { invoke_ptr_returned(address_conditional_return(c)); }
struct AddressCallbacks { Callback first; Callback second; };
void address_conditional_fields(int c) {
    AddressCallbacks callbacks{alpha, beta};
    invoke_ptr_fields(&(c ? callbacks.first : callbacks.second));
}
void address_conditional_derefs(int c) {
    Callback a = alpha, b = beta;
    Callback *pa = &a, *pb = &b;
    invoke_ptr_derefs(&(c ? *pa : *pb));
}
void address_conditional_nested(int c, int d) {
    Callback a = alpha, b = beta;
    invoke_ptr_nested(&(c ? (d ? a : b) : b));
}
void address_conditional_writes(int c) {
    Callback a = nullptr, b = nullptr;
    Callback *selected = &(c ? a : b);
    *selected = alpha;
    a(); b();
}

void conditional_wrapper_field_root(int c) {
    std::shared_ptr<Stream> a(new Stream(alpha)), b(new Stream(beta));
    std::shared_ptr<Noise> noise(new Noise{0, unrelated});
    (c ? a : b)->callback();
}
