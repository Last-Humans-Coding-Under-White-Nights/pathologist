// Value slice (#205): a callback member written by an IPC handler and read
// by a thread, the shape of the camera race fixes in #197 section 4.4, plus
// the stop rules and the cross-context flags around it.
#include <thread>

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};
IRemoteObject *Remote();

class ICallback {
public:
    virtual void OnError(int code) = 0;
};
class Listener : public ICallback {
public:
    void OnError(int code) override {}
};

// The proxy side of the IPC pair: its methods name the stub's handlers.
class IDeviceProxy {
public:
    int SetCallback(ICallback *cb);
    int Swap(ICallback *next);
};
int IDeviceProxy::SetCallback(ICallback *cb)
{
    void *data = 0, *reply = 0, *option = 0;
    Remote()->SendRequest(1, data, reply, option);
    return 0;
}
int IDeviceProxy::Swap(ICallback *next)
{
    void *data = 0, *reply = 0, *option = 0;
    Remote()->SendRequest(2, data, reply, option);
    return 0;
}

class IDeviceStub {
public:
    // IPC handler: writes the member.
    int SetCallback(ICallback *cb)
    {
        callback_ = cb;
        return 0;
    }
    // IPC handler, the only code touching `spare_`: two requests may run it
    // at once.
    int Swap(ICallback *next)
    {
        ICallback *old = spare_;
        spare_ = next;
        return old != nullptr;
    }
    // Thread entry: reads the member and calls through it.
    void Report(int code)
    {
        ICallback *cb = callback_;
        ICallback *alias = cb;
        alias->OnError(code);
        Notify(code);
    }
    // Reads the member only as the receiver of calls through it: each call
    // reads it.
    void Notify(int code)
    {
        callback_->OnError(code);
        callback_->OnError(code + 1);
    }
    void Start()
    {
        worker_ = std::thread(&IDeviceStub::Report, this, 1);
    }
    // A getter hands the one member to every caller.
    ICallback *Current()
    {
        return callback_;
    }
    void Probe1()
    {
        ICallback *p1 = Current();
    }
    void Probe2()
    {
        ICallback *p2 = Current();
    }
    ICallback *callback_ = nullptr;
    ICallback *spare_ = nullptr;
    std::thread worker_;
};

// A global written once and read elsewhere: a boundary when reached.
ICallback *g_listener = nullptr;
void InstallGlobal()
{
    g_listener = new Listener();
}
void FireGlobal(int code)
{
    ICallback *seen = g_listener;
    seen->OnError(code);
}

// A static data member used as a start.
class Registry {
public:
    static void Set(ICallback *cb) { current_ = cb; }
    static void Fire(int code)
    {
        ICallback *now = current_;
        now->OnError(code);
    }
    static ICallback *current_;
};
ICallback *Registry::current_ = nullptr;

// A buffer handed to a thread at its start.
struct Buffer {
    Buffer *next;
};
void Consume(Buffer *buf)
{
    buf->next = buf;
}
void Produce()
{
    Buffer *buf = new Buffer();
    std::thread t(Consume, buf);
    t.join();
}

// A thread that keeps its heap object to itself.
void Own()
{
    Buffer *mine = new Buffer();
    Buffer *also = mine;
    mine->next = also;
}
void StartOwn()
{
    std::thread t(Own);
    t.join();
}

// An allocating wrapper: each call returns its own object, which the
// context-insensitive heap merges into one.
Buffer *Alloc()
{
    Buffer *made = new Buffer();
    return made;
}
void UseA()
{
    Buffer *mineA = Alloc();
}
void UseB()
{
    Buffer *mineB = Alloc();
}

// A chain of copies for the depth limits.
void Chain(ICallback *a)
{
    ICallback *b = a;
    ICallback *c = b;
    ICallback *d = c;
    ICallback *e = d;
    e->OnError(0);
}

// A pass-through function hands its argument back to the call it came in
// through, and only to that call.
Buffer *Pass(Buffer *x)
{
    return x;
}
void Relay()
{
    Buffer *a = new Buffer();
    Buffer *b = Pass(a);
    b->next = nullptr;
    Buffer *c = a;
    c->next = c;
}
void Bystander()
{
    Buffer *other = Pass(nullptr);
    other->next = nullptr;
}

// A recursive walk nothing else calls: its code is `root`'s, and its
// parameter's only incoming value is its own.
class Walker {
public:
    void Walk(int n, ICallback *v)
    {
        hook_ = v;
        if (n)
            Walk(n - 1, v);
    }
    void Watch()
    {
        ICallback *seen_hook = hook_;
        seen_hook->OnError(0);
    }
    void Begin()
    {
        watcher_ = std::thread(&Walker::Watch, this);
    }
    ICallback *hook_ = nullptr;
    std::thread watcher_;
};

// Workers started in a loop: two of them may run `Work` at once.
class Pool {
public:
    void Work()
    {
        Buffer *got = new Buffer();
        last_ = got;
    }
    void Spawn()
    {
        for (int i = 0; i < 4; i++) {
            std::thread t(&Pool::Work, this);
            t.detach();
        }
    }
    Buffer *last_ = nullptr;
};

int main()
{
    IDeviceStub *stub = new IDeviceStub();
    ICallback *listener = new Listener();
    stub->SetCallback(listener);
    stub->Start();
    InstallGlobal();
    FireGlobal(1);
    Registry::Set(listener);
    Registry::Fire(2);
    Produce();
    StartOwn();
    Chain(listener);
    stub->Probe1();
    stub->Probe2();
    UseA();
    UseB();
    return 0;
}

// A request's options: a handler's local copy is its own, a copy on the heap
// is memory two requests may share.
struct MessageOption {
    ICallback *cb;
};
void Use(ICallback *cb);

class IOptionProxy {
public:
    int Post(ICallback *f);
};
int IOptionProxy::Post(ICallback *f)
{
    void *data = 0, *reply = 0, *option = 0;
    Remote()->SendRequest(1, data, reply, option);
    return 0;
}

class IOptionStub {
public:
    // IPC handler: `opt` is this request's alone; `held` is not.
    int Post(ICallback *f)
    {
        MessageOption opt;
        MessageOption *mine = &opt;
        mine->cb = f;
        Use(mine->cb);
        MessageOption *held = new MessageOption();
        held->cb = f;
        Use(held->cb);
        return 0;
    }
};
