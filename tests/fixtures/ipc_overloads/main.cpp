class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();

void HandleInt() {}
void HandleDouble() {}

class OverloadStub {
public:
    int Run(int value) {
        HandleInt();
        return value;
    }

    double Run(double value) {
        HandleDouble();
        return value;
    }
};

class OverloadProxy {
public:
    int Run(int value);
    double Run(double value);
};

int OverloadProxy::Run(int value) {
    IRemoteObject *remote = Remote();
    remote->SendRequest(1, nullptr, nullptr, nullptr);
    return value;
}

double OverloadProxy::Run(double value) {
    IRemoteObject *remote = Remote();
    remote->SendRequest(2, nullptr, nullptr, nullptr);
    return value;
}

int main() {
    OverloadProxy proxy;
    return proxy.Run(1) + (int)proxy.Run(2.0);
}
