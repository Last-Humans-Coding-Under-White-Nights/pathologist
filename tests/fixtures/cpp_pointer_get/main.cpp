namespace std {
template<class T> class shared_ptr {
public:
    T* get() const;
    T* get(int);
    T* operator->() const;
};
template<class T> class unique_ptr {
public:
    T* get() const;
    T* operator->() const;
};
template<class T, class A> shared_ptr<T> make_shared(A&&);
}
using Callback = void (*)();
void alpha() {}
void unrelated() {}
struct Handler { Callback callback; Handler *next; };
void control(std::shared_ptr<Handler> p) {
    p->callback = alpha;
    p->callback();
}
void extracted(std::shared_ptr<Handler> p) {
    p->callback = alpha;
    p.get()->callback();
}
void unique(std::unique_ptr<Handler> p) { p.get()->callback(); }
void qualified(::std::shared_ptr<Handler> p) {
    p.std::shared_ptr<Handler>::get()->callback();
}
void reference(const std::shared_ptr<Handler> &p) { p.get()->callback(); }
void unique_reference(const std::unique_ptr<Handler> &p) { p.get()->callback(); }
void pointer_receiver(std::shared_ptr<Handler> *p) { p->get()->callback(); }
void dereferenced_receiver(std::shared_ptr<Handler> *p) { (*p).get()->callback(); }
void const_pointee(std::shared_ptr<const Handler> p) { p.get()->callback(); }
struct Holder { std::unique_ptr<Handler> item; };
void field(Holder *h) { h->item.get()->callback(); }
std::shared_ptr<Handler> g_wrapper;
std::shared_ptr<Handler> wrapper() { return g_wrapper; }
void returned_wrapper() { wrapper().get()->callback(); }
void assignment(std::shared_ptr<Handler> p) {
    Handler *raw = p.get();
    Handler *assigned;
    assigned = p.get();
    raw->callback();
    assigned->callback();
}
Handler *return_raw(std::shared_ptr<Handler> p) { return p.get(); }
Handler *return_cast(std::unique_ptr<Handler> p) {
    return dynamic_cast<Handler*>(p.get());
}
void use_returns(std::shared_ptr<Handler> p, std::unique_ptr<Handler> u) {
    Handler *raw = return_raw(p);
    Handler *cast_raw = return_cast(u);
    raw->callback();
    cast_raw->callback();
}
void take_pointer(Handler *p) { p->callback(); }
void take_reference(const Handler &p) { p.callback(); }
void take_value(Handler p) { p.callback(); }
void arguments(std::shared_ptr<Handler> p) {
    take_pointer(p.get());
    take_reference(*(p.get()));
    take_value(*p.get());
}
void chained(std::shared_ptr<Handler> p) {
    auto next = p.get()->next;
    next->callback();
}
struct Repacked {
    Repacked(const Handler &event) { event.callback(); }
};
// HiView: extraction/dereference passed to a member, and to a factory's
// constructor. Task scheduling and bound-callable semantics are independent.
struct Queue { void ProcessUnorderedEvent(const Handler &event) { event.callback(); } };
void hiview_queue(Queue *queue, std::shared_ptr<Handler> event) {
    queue->ProcessUnorderedEvent(*(event.get()));
}
void hiview_repack(std::shared_ptr<Handler> base) {
    auto result = std::make_shared<Repacked>(*(base.get()));
}
// HDF: a named cast around extraction in a return, including a derived
// pointee. The cast does not invent an allocation or a load.
struct ConfigNode : Handler {};
ConfigNode *hdf_cast(const std::shared_ptr<Handler> &astObject) {
    return dynamic_cast<ConfigNode*>(astObject.get());
}
ConfigNode config;
void hdf_alias() {
    std::shared_ptr<Handler> astObject = &config;
    // Make the reference's address binding explicit; ordinary class-value
    // to reference argument wiring is independent of extraction semantics.
    std::shared_ptr<Handler> *address = &astObject;
    ConfigNode *raw = hdf_cast(*address);
}
// A user method and a nonzero-argument lookalike keep ordinary semantics.
struct User { Handler *get(); };
void user_get(User p) { Handler *user_raw = p.get(); }
void nonzero(std::shared_ptr<Handler> p) { Handler *not_extracted = p.get(0); }
namespace custom {
template<class T> struct shared_ptr { T *get(); T *operator->(); };
}
void custom_get(custom::shared_ptr<Handler> p) { Handler *custom_raw = p.get(); }

// Populate wrapper values through their storage; deliberately mix in an
// incompatible object and the wrapper's own address for the unwrap filter.
Handler concrete;
struct Other { Callback callback; } other;
void fill(std::shared_ptr<Handler> *out) { *out = &concrete; }
void contaminate(std::shared_ptr<Handler> *out) {
    *out = &other;
    *out = out;
}
void concrete_alias() {
    std::shared_ptr<Handler> p;
    fill(&p);
    contaminate(&p);
    Handler *filtered = p.get();
    void *erased = p.get();
}
