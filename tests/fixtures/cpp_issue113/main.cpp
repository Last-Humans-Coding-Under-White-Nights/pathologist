void callback() {}
void callback_alt() {}
void callback_arg(int) {}
#include "holder.h"
#include "remote.h"
namespace ffrt { struct queue { template<class F> void submit(F); }; }
namespace elsewhere { struct Service { static void Write() {} }; }
namespace app {
struct Service {
    static Service& GetInstance();
    static Service* GetPointer();
    void Start() {}
    static void Write() {}
};
void relative() { Service::Write(); }
void chained() { Service::GetInstance().Start(); }
void chained_pointer() { Service::GetPointer()->Start(); }
void queued(ffrt::queue& queue) { queue.submit([] { Service::Write(); }); }
void queued_variable(ffrt::queue& queue) {
    auto task = [] { Service::Write(); };
    queue.submit(task);
}
void custom_submit(void (*)());
void defined_submit(void (*)()) {}
void configured() { custom_submit(callback); defined_submit(callback); }
void queued_arg(ffrt::queue& queue) { queue.submit(callback_arg); }
// Every function the argument may hold is a callback the queue may run.
using Task = void (*)();
void queued_either(ffrt::queue& queue, bool c) {
    Task task = callback;
    if (c) { task = callback_alt; }
    queue.submit(task);
}
void unrelated(void (*store)(void (*)())) { store([] { Service::Write(); }); }
void global_qualified() { ::elsewhere::Service::Write(); }
struct Other { void Start() {} };
Service make(int);
Other make(double);
void cast_chain(int i) { make((double)i).Start(); make(((double)i)).Start(); }
struct Chain { Chain next(); void Finish() {} };
Chain chain();
void long_chain() {
    chain().next().next().next().next().next().next().next().next()
        .next().next().next().next().next().next().next().next()
        .next().next().next().next().next().next().next().next().Finish();
}
struct Fixture { Service* service; };
HWTEST_F(Fixture, CallsMember, Level0) { service->Start(); }
HWTEST_P(Fixture, ParamCallsMember, Level0) { service->Start(); }
HWTEST(Standalone, CallsGlobal, Level0) { Service::Write(); }
template<class T> struct Box {};
template<class T> struct Mixed {
    T Get(int) { return T(); }
    Box<T> Get(double) { return Box<T>(); }
};
template<class T> struct MixedAuto {
    T Get(int) { return T(); }
    auto Get(double) { return Other(); }
};
void mixed_auto_return(MixedAuto<Service>& h) { auto result = h.Get(1.0); result.Start(); }
void mixed_return(Mixed<Service>& h) { auto result = h.Get(1.0); result.Start(); }
void nested_holder(Holder<Holder<Holder<Holder<Service>>>> h) { h.Get().Get().Get().Get().Start(); }
// A receiver's pointer layers name the same class template as its value.
void holder_pointer(Holder<Service *> *h) { h->Get()->Start(); }
// `->` on a receiver whose spelling keeps its template arguments.
void holder_arrow(Holder<Service *> *h) { h->Ping(); }
// A member inherited from an instantiated class template substitutes the
// base's arguments, not the derived class's own (it has none).
struct Adopted : Holder<Service *> {
    void Run() { Get()->Start(); }
};
// The base that carries the arguments may be several classes up.
struct Middle : Holder<Service *> {};
struct Far : Middle {
    void Run() { Get()->Start(); }
};
struct Base { virtual void Filter() {} };
struct Video : Base { void Filter() override {} };
struct Adapter {
    Holder<Base*> holder;
    void Filter() { auto controller = holder.Get(); controller->Filter(); }
};
}
using namespace app;
// A `using namespace` directive carries a qualified call, as it already
// carried a bare one.
void via_using() { Service::Write(); }
using namespace rem;
// Across translation units: `remote.cpp` defines `Remote::Call` under the
// same directive, which names `rem::Remote::Call` — the header's prototype —
// so the call here reaches that body.
void via_using_remote() { Remote::Call(); }
