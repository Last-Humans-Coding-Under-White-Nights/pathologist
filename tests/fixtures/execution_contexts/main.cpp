// Execution contexts: every callback a modelled callee runs (a thread start,
// a task submission) and every IPC stub handler is a place where an
// execution context starts. Each entry below is started at one site.
#include <thread>
#include <pthread.h>

namespace ffrt { struct queue { template<class F> void submit(F); }; }

// Started once, with nothing that suggests more than one instance.
void *Worker(void *arg) { return arg; }
void StartPthread(void *ctx) {
    pthread_t tid;
    pthread_create(&tid, nullptr, Worker, ctx);
}

void Rotate(int *pic) {}
void StartThread(int *pic) { std::thread t(Rotate, pic); t.detach(); }

// Started in a loop body: one instance per iteration.
void *Child(void *arg) { return arg; }
void SpawnChild(void *arg) {
    pthread_t tid;
    pthread_create(&tid, nullptr, Child, arg);
}
void *LoopWorker(void *arg) { SpawnChild(arg); return arg; }
void StartLoop(void *ctx, int n) {
    for (int i = 0; i < n; i++) {
        pthread_t tid;
        pthread_create(&tid, nullptr, LoopWorker, ctx);
    }
}

void Tick() {}
void StartTicks(bool more) {
    while (more) {
        std::thread t(Tick);
        t.join();
    }
}

// Started from a function in a call-graph cycle (recursion).
void Again(int depth);
void Recurse() {}
void Respawn(int depth) {
    std::thread t(Recurse);
    t.join();
    if (depth > 0) { Again(depth); }
}
void Again(int depth) { Respawn(depth - 1); }

// A wrapper every thread is started through, as `OsalThreadCreate` is: one
// start site for each entry. A thread that starts another through it closes
// a cycle through the start edges, which is not recursion.
void *Leaf(void *arg) { return arg; }
void Create(void *(*entry)(void *), void *arg) {
    pthread_t tid;
    pthread_create(&tid, nullptr, entry, arg);
}
void *Nest(void *arg) { Create(Leaf, arg); return arg; }
void StartNest() { Create(Nest, nullptr); }

// A serial task queue.
void Serial() {}
void Enqueue(ffrt::queue &queue) { queue.submit(Serial); }

// Project wrappers described by `models.toml`: one says what it starts, one
// does not.
void pool_post(void (*job)(void));
void run_later(void (*job)(void));
void PoolJob() {}
void Later() {}
void StartPool() { pool_post(PoolJob); }
void StartLater() { run_later(Later); }

// An IPC proxy/stub pair: the stub handler runs on the IPC worker pool, and a
// thread it starts may be started once per request.
class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};
IRemoteObject *Remote();

void *IpcChild(void *arg) { return arg; }
class IFooStub {
public:
    int HandleGetInfo(int key) {
        pthread_t tid;
        pthread_create(&tid, nullptr, IpcChild, nullptr);
        return key;
    }
};

class IFooProxy {
public:
    int GetInfo(int key);
};

int IFooProxy::GetInfo(int key) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(1, data, reply, option);
    return key;
}
