// Callback interface (mirrors AbilityConnectionWrapperStub/Proxy).

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();

class ConnectionStub {
public:
    void OnConnect(int id) { (void)id; }
    void OnDisconnect(int id) { (void)id; }
};

class ConnectionProxy {
public:
    void OnConnect(int id);
    void OnDisconnect(int id);
};

void ConnectionProxy::OnConnect(int id) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(10, data, reply, option);
    (void)id;
}
void ConnectionProxy::OnDisconnect(int id) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(11, data, reply, option);
    (void)id;
}

int main() {
    ConnectionProxy p;
    p.OnConnect(1);
    ConnectionStub s;
    s.OnDisconnect(2);
    return 0;
}
