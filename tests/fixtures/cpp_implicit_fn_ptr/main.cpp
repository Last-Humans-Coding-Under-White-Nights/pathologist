struct Handler {
    void (*fn)();
};

void target_callback() {}

class Dispatcher {
    Handler *handler_ = nullptr;
public:
    void Init(Handler *h) {
        // Unqualified member variable assignment produces zero flow constraints
        // during lowering; this test exercises the Andersen field summary fallback
        // until unqualified member assignments land.
        handler_ = h;
    }
    void Dispatch() {
        handler_->fn();
    }
};

static Handler g_handler = { target_callback };

void run() {
    Dispatcher d;
    d.Init(&g_handler);
    d.Dispatch();
}
