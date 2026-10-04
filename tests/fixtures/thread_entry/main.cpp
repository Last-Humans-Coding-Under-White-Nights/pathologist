// Thread entry points: the started function is reachable from the code that
// starts it, and the forwarded arguments reach its parameters.
#include <thread>
#include <pthread.h>

typedef void (*Handler)(void);
struct Ctx { Handler h; };

void OnEvent(void) {}
void OnThreadEvent(void) {}
void OnLoopEvent(void) {}
void OnEntryEvent(void) {}
void OnLambdaEvent(void) {}
void OnPoolEvent(void) {}
void OnDirectEvent(void) {}

void Rotate(int *pic) {}
void Idle(void) {}
void *Worker(void *arg) { static_cast<Ctx *>(arg)->h(); return nullptr; }
void *Drain(void *arg) { ((Ctx *)arg)->h(); return nullptr; }
void *Call(void *arg) { ((Handler)arg)(); return nullptr; }
void Run(Handler h) { h(); }
void Loop(int id, Handler h) { h(); }
void Entry(Handler h) { h(); }

void StartThread(int *pic) { std::thread t(Rotate, pic); t.detach(); }

void StartPthread(Ctx *ctx) {
    ctx->h = OnEvent;
    pthread_t tid;
    pthread_create(&tid, nullptr, Worker, ctx);
}

void StartDrain(Ctx *ctx) {
    pthread_t tid;
    pthread_create(&tid, nullptr, Drain, ctx);
}

// The start routine's argument is the function it calls.
void StartCall() {
    pthread_t tid;
    pthread_create(&tid, nullptr, Call, (void *)OnDirectEvent);
}

void StartRun() { std::thread t(Run, OnThreadEvent); t.join(); }

// The variadic rest: every argument past the callable is forwarded in order.
void StartLoop() {
    Handler h = OnLoopEvent;
    std::thread t(Loop, 7, h);
    t.join();
}

// The callable held in a variable.
typedef void (*EntryFn)(Handler);
void StartEntry() {
    EntryFn entry = Entry;
    std::thread t(entry, OnEntryEvent);
    t.join();
}

// A temporary assigned to an existing thread object.
struct Service {
    std::thread task;
    void Process(int *pic) { task = std::thread(Rotate, pic); }
};

void StartLambda() {
    std::thread t([](Handler h) { h(); }, OnLambdaEvent);
    t.join();
}

void StartNoArgs() { std::thread t(Idle); t.join(); }

// A project wrapper described by a user model (`models.toml`).
void pool_post(int priority, void (*task)(Handler), Handler h);
void PoolTask(Handler h) { h(); }
void StartPool() { pool_post(0, PoolTask, OnPoolEvent); }

// Member functions as entry points: a static one takes the forwarded
// arguments past its `this` slot, a non-static one takes its receiver first.
void OnStaticEvent(void) {}
void OnStaticEntryEvent(void) {}
void OnMemberEvent(void) {}
struct Svc {
    static void Out(Handler h);
    static void *Entry(void *arg);
    void Run(Handler h);
    void StartRun();
};
void Svc::Out(Handler h) { h(); }
void *Svc::Entry(void *arg) { ((Handler)arg)(); return nullptr; }
void Svc::Run(Handler h) { h(); }
void Svc::StartRun() { std::thread t(&Svc::Run, this, OnMemberEvent); t.join(); }
void StartStatic() { std::thread t(Svc::Out, OnStaticEvent); t.join(); }
void StartStaticEntry() {
    pthread_t tid;
    pthread_create(&tid, nullptr, Svc::Entry, (void *)OnStaticEntryEvent);
}

// A callable that cannot take the arguments passed is not started.
void OnPairEvent(void) {}
void Pair(Handler a, Handler b) { b(); }
typedef void (*PairFn)(Handler, Handler);
void StartPair(bool c) {
    PairFn entry = Pair;
    if (c) { entry = (PairFn)Idle; }
    std::thread t(entry, OnPairEvent, OnPairEvent);
    t.join();
}

// A defaulted parameter need not be passed.
void OnDefaultEvent(void) {}
void PoolDefault(Handler h, int retries = 3) { h(); }
void pool_post_default(int priority, void (*task)(Handler, int), Handler h);
void StartPoolDefault() { pool_post_default(0, PoolDefault, OnDefaultEvent); }

// Cast spellings of a context argument.
typedef Ctx *CtxPtr;
struct Node { void *data; Handler h; };
void *ViaTypedef(void *arg) { ((CtxPtr)arg)->h(); return nullptr; }
void *ViaConst(void *arg) { ((Ctx *const)arg)->h(); return nullptr; }
void ViaSameClass(Ctx *ctx) { static_cast<Ctx *>(ctx)->h(); }
void ViaMember(Node *node) { ((Ctx *)node->data)->h(); }
