// if/else-if dispatch (mirrors ThermalLevelCallbackStub/Proxy).
// Interface: IThermalStub/IThermalProxy. Proxy/stub methods share names.

class IRemoteObject {
public:
    int SendRequest(int code, void *data, void *reply, void *option);
};

IRemoteObject *Remote();

class IThermalStub {
public:
    int OnTemperatureChanged(int t) { (void)t; return 1; }
    int OnLevelChanged(int l) { (void)l; return 2; }
};

class IThermalProxy {
public:
    int OnTemperatureChanged(int t);
    int OnLevelChanged(int l);
};

int IThermalProxy::OnTemperatureChanged(int t) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(3, data, reply, option);
    return t;
}

int IThermalProxy::OnLevelChanged(int l) {
    IRemoteObject *remote = Remote();
    void *data = 0, *reply = 0, *option = 0;
    remote->SendRequest(4, data, reply, option);
    return l;
}

int main() {
    IThermalProxy p;
    int t = p.OnTemperatureChanged(23);
    IThermalStub s;
    int l = s.OnLevelChanged(5);
    return t + l;
}
