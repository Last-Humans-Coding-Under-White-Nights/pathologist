// Handler named with a `Stub` suffix only (the marshalling-shim name some IDL
// generators use, e.g. ThermalLevelCallbackStub::OnThermalLevelChangedStub).
// Proxy `OnFoo` must bridge to stub `FooStub` handler `OnFooStub`.

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();

class FooStub {
public:
    int OnFooStub(int v) { (void)v; return 1; } // only a `Stub`-suffixed handler, no plain `OnFoo`
    int OnBarStub(int v) { (void)v; return 2; }
};

class FooProxy {
public:
    int OnFoo(int v);
    int OnBar(int v);
};

int FooProxy::OnFoo(int v) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(0, data, reply, option);
    return v;
}
int FooProxy::OnBar(int v) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(1, data, reply, option);
    return v;
}

int main() {
    FooProxy p;
    int a = p.OnFoo(1);
    FooStub s;
    int b = s.OnBarStub(2);
    return a + b;
}
