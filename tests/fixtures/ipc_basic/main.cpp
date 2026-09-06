// Hand-written proxy/stub pair (mirrors FaultLoggerStub/Proxy).
// Interface: IFooStub/IFooProxy. Proxy methods call SendRequest.

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();
void LogSendRequestResult() {}

class IFooStub {
public:
    int HandleGetInfo(int key) { (void)key; return 1; }
    int HandleSetInfo(int key, int val) { (void)key; (void)val; return 2; }
    int LocalOnly() { return 3; }
};

class IFooProxy {
public:
    int GetInfo(int key);
    int SetInfo(int key, int val);
    int LocalOnly();
};

int IFooProxy::GetInfo(int key) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(1, data, reply, option); // opcode 1 = GET_INFO
    return key;
}

int IFooProxy::SetInfo(int key, int val) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(2, data, reply, option); // opcode 2 = SET_INFO
    return val;
}

int IFooProxy::LocalOnly() {
    LogSendRequestResult(); // Contains the substring, but is not an IPC send.
    return 3; // Same-named stub method, but no IPC send.
}

int main() {
    IFooProxy p;
    int a = p.GetInfo(10);
    IFooStub s;
    int b = s.HandleGetInfo(10);
    return a + b;
}
