// #121 and #122 together: a singleton client calls through an interface
// into a proxy, whose SendRequest is bridged to the service behind the stub.
template <typename T> class DelayedSingleton {
public:
    static std::shared_ptr<T> GetInstance();
};
template <typename T> class sptr {
public:
    T *operator->() const;
};
class MessageParcel {};
class IRemoteObject {
public:
    int SendRequest(int code, MessageParcel &data, MessageParcel &reply);
};

struct IFoo {
    virtual int Start(int x) = 0;
};
template <typename T> class IRemoteProxy;
template <typename T> class IRemoteStub;

class FooProxy : public IRemoteProxy<IFoo> {
public:
    int Start(int x) override
    {
        MessageParcel data, reply;
        return remote_->SendRequest(1, data, reply);
    }

private:
    IRemoteObject *remote_;
};

class FooStub : public IRemoteStub<IFoo> {
public:
    int OnRemoteRequest(int code) { return Start(code); }
};

class FooService : public FooStub {
public:
    int Start(int x) override { return x; }
};

class FooClient : public DelayedSingleton<FooClient> {
public:
    int Launch(int x) { return proxy_->Start(x); }

private:
    sptr<IFoo> proxy_;
};

void app() { FooClient::GetInstance()->Launch(1); }
