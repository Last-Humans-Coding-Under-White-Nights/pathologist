// Opcodes as enum members (mirrors FaultLoggerServiceInterfaceCode).
// Proxy/stub interface whose methods are matched by name.

enum class FooInterfaceCode {
    ADD = 0,
    QUERY = 1,
    DESTROY = 5,
};

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();

class FooStub {
public:
    int Add(int a) { (void)a; return 1; }
    int Query(int q) { (void)q; return 2; }
    int Destroy() { return 3; }
};

class FooProxy {
public:
    int Add(int a);
    int Query(int q);
    int Destroy();
};

int FooProxy::Add(int a) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest((int)FooInterfaceCode::ADD, data, reply, option);
    return a;
}
int FooProxy::Query(int q) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest((int)FooInterfaceCode::QUERY, data, reply, option);
    return q;
}
int FooProxy::Destroy() {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest((int)FooInterfaceCode::DESTROY, data, reply, option);
    return 0;
}

int main() {
    FooProxy p;
    int a = p.Add(1);
    FooStub s;
    int d = s.Destroy();
    return a + d;
}
